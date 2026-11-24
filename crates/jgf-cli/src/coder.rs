//! The coder step of an end-to-end run.
//!
//! A [`CoderRunner`] receives the prompt (frozen spec, plan and task branch) and edits the
//! scratch work tree. [`ClaudeCoderRunner`] runs `claude -p` under bwrap with a hard wall-clock
//! limit, a turn cap and a dollar budget; [`FakeCoderRunner`] is the dry-run and test double.
//!
//! Filesystem access is isolated via bwrap: the host filesystem is not visible except for the
//! listed binds; the task work tree is writable except for its read-only `.git` directory, and
//! scratch is writable, while system directories and the single Claude credentials file are
//! read-only. `/tmp` is a fresh tmpfs, and `/proc` and `/dev` are fresh views. Network access
//! remains open, so the child can still reach whatever the network allows with the credentials it
//! has: Claude auth only. Full `jg-sandbox` integration is a follow-up. No code here reads a
//! secret store.

use std::{
    fs,
    io::Read,
    path::PathBuf,
    process::Stdio,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

use crate::{
    child,
    e2e::KillSwitch,
    scratch::{ScratchError, ScratchHome},
};

/// File the dry-run and test coder writes to prove it changed the work tree.
pub const FAKE_EDIT_FILE: &str = "CODER_NOTE.md";

/// How often a running coder is checked for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One coder invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct CoderRequest {
    /// State directory of the run; it is never mounted into the child.
    pub state_dir: PathBuf,
    /// Prompt sent to the coder; it carries the spec, the plan and the task branch name.
    pub prompt: String,
    /// Work tree the coder edits.
    pub work_dir: PathBuf,
    /// Scratch home the child sees as `HOME`; it holds only Claude auth.
    pub scratch_dir: PathBuf,
    /// Hard wall-clock limit of the process.
    pub timeout: Duration,
    /// Maximum number of agentic turns.
    pub max_turns: u32,
    /// Maximum amount of US dollars to spend.
    pub max_budget_usd: f64,
}

impl CoderRequest {
    /// The fixed argument vector used by the real runner and recorded in the audit log.
    pub fn command(&self) -> Vec<String> {
        vec![
            "claude".to_string(),
            "-p".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--no-session-persistence".to_string(),
            "--max-turns".to_string(),
            self.max_turns.to_string(),
            "--max-budget-usd".to_string(),
            format!("{:.2}", self.max_budget_usd),
            self.prompt.clone(),
        ]
    }
}

/// What one coder process did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CoderRun {
    /// Exit code when the process exited normally.
    pub exit_code: Option<i32>,
    /// Signal that ended the process, when it was killed.
    pub signal: Option<i32>,
    /// Whether the hard wall-clock limit killed the process.
    pub timed_out: bool,
    /// Whether the kill switch killed the process.
    pub killed: bool,
    /// Whether the coder stopped because its dollar budget ran out.
    pub budget_exhausted: bool,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Why the process could not be started, when it could not.
    pub spawn_error: Option<String>,
}

impl CoderRun {
    /// Whether the coder exited successfully within its limits.
    pub fn succeeded(&self) -> bool {
        self.spawn_error.is_none()
            && !self.killed
            && !self.timed_out
            && !self.budget_exhausted
            && self.exit_code == Some(0)
    }

    /// A short reason for an unsuccessful run.
    pub fn failure_reason(&self) -> String {
        let summary = if let Some(error) = &self.spawn_error {
            format!("could not start the coder: {error}")
        } else if self.killed {
            "coder was killed by the kill switch".to_string()
        } else if self.timed_out {
            "coder hit its wall-clock limit".to_string()
        } else if self.budget_exhausted {
            "coder hit its dollar budget".to_string()
        } else if let Some(signal) = self.signal {
            format!("coder exited after signal {signal}")
        } else {
            let code = self
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".to_string());
            format!("coder exited with status {code}")
        };
        child::failure_reason(&summary, &self.stdout, &self.stderr)
    }
}

/// Runs one coder request.
pub trait CoderRunner {
    /// Runs `request` and returns what the process did.
    fn run(&self, request: &CoderRequest) -> CoderRun;

    /// Runs `request`, stopping and reaping the child as soon as `kill` is set.
    ///
    /// The default ignores the kill switch, which keeps simple doubles simple.
    fn run_with_kill(&self, request: &CoderRequest, kill: &dyn KillSwitch) -> CoderRun {
        let _ = kill;
        self.run(request)
    }
}

/// The real coder: a `claude -p` process in the work tree.
#[derive(Debug, Clone)]
pub struct ClaudeCoderRunner {
    binary: PathBuf,
    bwrap_binary: PathBuf,
    real_home: Option<PathBuf>,
}

impl ClaudeCoderRunner {
    /// A runner that starts the installed `claude` command.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            bwrap_binary: PathBuf::from(child::BWRAP_BINARY),
            real_home: None,
        }
    }

    /// A runner that starts `binary` and finds the host credentials under `home`.
    ///
    /// Tests use a script and a fake home.
    #[cfg(test)]
    pub(crate) fn with_binary_and_home(
        binary: impl Into<PathBuf>,
        home: impl Into<PathBuf>,
    ) -> Self {
        Self {
            binary: binary.into(),
            bwrap_binary: PathBuf::from(child::BWRAP_BINARY),
            real_home: Some(home.into()),
        }
    }

    fn scratch_home(&self, request: &CoderRequest) -> Result<ScratchHome, ScratchError> {
        match &self.real_home {
            Some(home) => ScratchHome::prepare(&request.scratch_dir, home),
            None => ScratchHome::prepare(&request.scratch_dir, &crate::scratch::real_home()?),
        }
    }

    fn run_inner(&self, request: &CoderRequest, kill: Option<&dyn KillSwitch>) -> CoderRun {
        let argv = request.command();
        let started = Instant::now();

        let scratch = match self.scratch_home(request) {
            Ok(scratch) => scratch,
            Err(error) => {
                return CoderRun {
                    spawn_error: Some(error.to_string()),
                    duration_ms: elapsed_ms(started),
                    ..CoderRun::default()
                };
            }
        };

        let mut command = match child::sandbox_command(
            &self.bwrap_binary,
            &self.binary,
            &argv,
            &request.state_dir,
            &request.work_dir,
            &scratch,
        ) {
            Ok(command) => command,
            Err(error) => {
                return CoderRun {
                    spawn_error: Some(error),
                    duration_ms: elapsed_ms(started),
                    ..CoderRun::default()
                };
            }
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        child::own_process_group(&mut command);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                return CoderRun {
                    spawn_error: Some(format!(
                        "could not start bwrap at {}: {error}",
                        self.bwrap_binary.display()
                    )),
                    duration_ms: elapsed_ms(started),
                    ..CoderRun::default()
                };
            }
        };

        let stdout_reader = child.stdout.take().map(spawn_reader);
        let stderr_reader = child.stderr.take().map(spawn_reader);

        let mut timed_out = false;
        let mut killed = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(error) => {
                    let _ = child::kill_process_group(&mut child);
                    return CoderRun {
                        spawn_error: Some(error.to_string()),
                        duration_ms: elapsed_ms(started),
                        ..CoderRun::default()
                    };
                }
            }
            if kill.is_some_and(|kill| kill.killed()) {
                killed = true;
                break child::kill_process_group(&mut child);
            }
            if started.elapsed() >= request.timeout {
                timed_out = true;
                break child::kill_process_group(&mut child);
            }
            thread::sleep(POLL_INTERVAL);
        };

        let stdout = join_reader(stdout_reader);
        let stderr = join_reader(stderr_reader);
        let spawn_error = child::bwrap_start_error(&stderr);
        let exit_code = status.and_then(|status| status.code());
        let signal = status.and_then(|status| {
            use std::os::unix::process::ExitStatusExt;
            status.signal()
        });
        let failed = !timed_out && exit_code != Some(0) && signal.is_none();
        let budget_exhausted = failed && mentions_budget(&stdout, &stderr);

        CoderRun {
            exit_code,
            signal,
            timed_out,
            killed,
            budget_exhausted,
            stdout,
            stderr,
            duration_ms: elapsed_ms(started),
            spawn_error,
        }
    }
}

impl Default for ClaudeCoderRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CoderRunner for ClaudeCoderRunner {
    fn run(&self, request: &CoderRequest) -> CoderRun {
        self.run_inner(request, None)
    }

    fn run_with_kill(&self, request: &CoderRequest, kill: &dyn KillSwitch) -> CoderRun {
        self.run_inner(request, Some(kill))
    }
}

/// A runner with a fixed behaviour, used by dry runs and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FakeCoderBehavior {
    /// Write a small file and succeed.
    #[default]
    Edit,
    /// Exit with a failure.
    Fail,
    /// Report that the wall-clock limit was hit.
    Timeout,
    /// Report that the dollar budget was spent.
    BudgetExhausted,
}

/// A [`CoderRunner`] that edits `CODER_NOTE.md`, fails, times out or spends its budget.
#[derive(Debug, Default)]
pub struct FakeCoderRunner {
    behavior: FakeCoderBehavior,
    requests: Mutex<Vec<CoderRequest>>,
}

impl FakeCoderRunner {
    /// A runner with `behavior`.
    pub fn new(behavior: FakeCoderBehavior) -> Self {
        Self {
            behavior,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// A runner that makes a trivial edit, as a dry run needs.
    pub fn editing() -> Self {
        Self::new(FakeCoderBehavior::Edit)
    }

    /// Requests seen so far, in order.
    pub fn requests(&self) -> Vec<CoderRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl CoderRunner for FakeCoderRunner {
    fn run(&self, request: &CoderRequest) -> CoderRun {
        let attempt = {
            let mut requests = self
                .requests
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            requests.push(request.clone());
            requests.len()
        };

        match self.behavior {
            FakeCoderBehavior::Edit => match fs::write(
                request.work_dir.join(FAKE_EDIT_FILE),
                format!("The fake coder made edit {attempt}.\n"),
            ) {
                Ok(()) => CoderRun {
                    exit_code: Some(0),
                    ..CoderRun::default()
                },
                Err(error) => CoderRun {
                    spawn_error: Some(error.to_string()),
                    ..CoderRun::default()
                },
            },
            FakeCoderBehavior::Fail => CoderRun {
                exit_code: Some(1),
                stderr: "fake coder failed".to_string(),
                ..CoderRun::default()
            },
            FakeCoderBehavior::Timeout => CoderRun {
                timed_out: true,
                ..CoderRun::default()
            },
            FakeCoderBehavior::BudgetExhausted => CoderRun {
                budget_exhausted: true,
                ..CoderRun::default()
            },
        }
    }
}

fn spawn_reader<R>(mut pipe: R) -> thread::JoinHandle<Vec<u8>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = pipe.read_to_end(&mut bytes);
        bytes
    })
}

fn join_reader(reader: Option<thread::JoinHandle<Vec<u8>>>) -> String {
    let bytes = reader
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn mentions_budget(stdout: &str, stderr: &str) -> bool {
    let text = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    text.contains("exceeded usd budget") || text.contains("budget exceeded")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::{
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use super::*;

    fn request(dir: &Path) -> CoderRequest {
        CoderRequest {
            state_dir: dir.to_path_buf(),
            prompt: "implement the plan".to_string(),
            work_dir: dir.to_path_buf(),
            scratch_dir: dir.join("scratch"),
            timeout: Duration::from_secs(30),
            max_turns: 7,
            max_budget_usd: 1.5,
        }
    }

    /// A fake host home holding a placeholder credentials file the tests never read.
    fn fake_home(root: &Path) -> PathBuf {
        let home = root.join("host-home");
        fs::create_dir_all(home.join(".claude")).expect("host claude dir");
        fs::write(
            home.join(".claude").join(".credentials.json"),
            b"placeholder",
        )
        .expect("fake credentials");
        home
    }

    fn script(root: &Path, name: &str, body: &str) -> PathBuf {
        let path = root.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("make executable");
        path
    }

    fn env_value<'a>(dump: &'a str, key: &str) -> Option<&'a str> {
        dump.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
    }

    #[test]
    fn the_command_carries_the_limits_and_the_prompt() {
        let dir = std::env::temp_dir();
        let command = request(&dir).command();

        assert_eq!(command[0], "claude");
        assert_eq!(command[1], "-p");
        assert!(command.iter().any(|arg| arg == "--max-turns"));
        assert!(command.iter().any(|arg| arg == "7"));
        assert!(command.iter().any(|arg| arg == "--max-budget-usd"));
        assert!(command.iter().any(|arg| arg == "1.50"));
        assert_eq!(
            command.last().map(String::as_str),
            Some("implement the plan")
        );
    }

    #[test]
    fn the_child_sees_a_scratch_home_without_gh_ssh_git_or_token_vars() {
        let root = tempfile::TempDir::new().expect("temp dir");
        fs::create_dir(root.path().join(".git")).expect("work-tree git directory");
        let home = fake_home(root.path());
        let dump = root.path().join("child.env");
        let binary = script(
            root.path(),
            "fake-claude",
            &format!("env > '{}'", dump.display()),
        );
        let runner = ClaudeCoderRunner::with_binary_and_home(&binary, &home);

        let run = runner.run(&request(root.path()));
        assert!(run.succeeded(), "{:?}", run.failure_reason());

        let env = fs::read_to_string(&dump).expect("child env");
        let scratch = root.path().join("scratch");
        assert_eq!(
            env_value(&env, "HOME"),
            Some(scratch.join("home").to_str().unwrap())
        );
        assert_eq!(
            env_value(&env, "XDG_CONFIG_HOME"),
            Some(scratch.join("config").to_str().unwrap())
        );
        assert_eq!(
            env_value(&env, "XDG_DATA_HOME"),
            Some(scratch.join("data").to_str().unwrap())
        );
        assert_eq!(
            env_value(&env, "XDG_CACHE_HOME"),
            Some(scratch.join("cache").to_str().unwrap())
        );
        assert_eq!(
            env_value(&env, "GH_CONFIG_DIR"),
            Some(scratch.join("gh").to_str().unwrap())
        );
        assert_eq!(
            env_value(&env, "CLAUDE_CONFIG_DIR"),
            Some(scratch.join("claude").to_str().unwrap())
        );
        assert_eq!(env_value(&env, "GIT_CONFIG_GLOBAL"), Some("/dev/null"));
        assert_eq!(env_value(&env, "GIT_CONFIG_SYSTEM"), Some("/dev/null"));
        assert_eq!(env_value(&env, "GIT_TERMINAL_PROMPT"), Some("0"));
        for key in ["GITHUB_TOKEN", "GH_TOKEN", "SSH_AUTH_SOCK", "GIT_ASKPASS"] {
            assert_eq!(env_value(&env, key), None, "{key} reached the child");
        }
        assert!(
            fs::symlink_metadata(scratch.join("claude").join(".credentials.json"))
                .expect("credentials link")
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn a_missing_bwrap_binary_fails_closed_with_a_clear_error() {
        let root = tempfile::TempDir::new().expect("temp dir");
        fs::create_dir(root.path().join(".git")).expect("work-tree git directory");
        let home = fake_home(root.path());
        let binary = script(root.path(), "fake-claude", "exit 0");
        let mut runner = ClaudeCoderRunner::with_binary_and_home(&binary, &home);
        runner.bwrap_binary = root.path().join("missing-bwrap");

        let run = runner.run(&request(root.path()));

        assert!(!run.succeeded());
        assert!(
            run.failure_reason().contains("could not start bwrap"),
            "{}",
            run.failure_reason()
        );
    }

    #[test]
    fn a_kill_switch_stops_the_whole_process_group() {
        let root = tempfile::TempDir::new().expect("temp dir");
        fs::create_dir(root.path().join(".git")).expect("work-tree git directory");
        let home = fake_home(root.path());
        let descendant_file = root.path().join("descendant.txt");
        let kill_path = crate::e2e::kill_file(root.path());
        fs::create_dir_all(kill_path.parent().expect("kill parent")).expect("kill dir");
        let binary = script(
            root.path(),
            "fake-claude",
            &format!(
                "(while :; do echo x >> '{}'; sleep 0.01; done) &\ntouch '{}'\nwait",
                descendant_file.display(),
                kill_path.display()
            ),
        );
        let runner = ClaudeCoderRunner::with_binary_and_home(&binary, &home);
        let kill = crate::e2e::FileKillSwitch::new(root.path());

        let run = runner.run_with_kill(&request(root.path()), &kill);

        assert!(run.killed);
        let written = fs::read(&descendant_file).expect("descendant writes");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fs::read(&descendant_file)
                .expect("descendant output remains")
                .len(),
            written.len(),
            "descendant kept running after its process group was killed"
        );
    }

    #[test]
    fn the_fake_edit_writes_a_file_and_succeeds() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let runner = FakeCoderRunner::editing();

        let run = runner.run(&request(root.path()));

        assert!(run.succeeded());
        assert!(root.path().join(FAKE_EDIT_FILE).is_file());
        assert_eq!(runner.requests().len(), 1);
    }

    #[test]
    fn the_fake_failures_report_their_reason() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let fail = FakeCoderRunner::new(FakeCoderBehavior::Fail);
        let timeout = FakeCoderRunner::new(FakeCoderBehavior::Timeout);
        let budget = FakeCoderRunner::new(FakeCoderBehavior::BudgetExhausted);

        assert!(
            fail.run(&request(root.path()))
                .failure_reason()
                .contains("fake coder failed")
        );
        assert!(
            timeout
                .run(&request(root.path()))
                .failure_reason()
                .contains("wall-clock")
        );
        assert!(
            budget
                .run(&request(root.path()))
                .failure_reason()
                .contains("dollar budget")
        );
    }

    #[test]
    fn a_failed_child_reason_includes_scrubbed_stdout_and_stderr() {
        let fake_token = format!("ghp_{}", "B".repeat(20));
        let run = CoderRun {
            exit_code: Some(1),
            stdout: format!("max turns reached; {fake_token}"),
            stderr: "request failed: fake stderr detail".to_string(),
            ..CoderRun::default()
        };

        let reason = run.failure_reason();

        assert!(reason.contains("coder exited with status 1"), "{reason}");
        assert!(reason.contains("stdout: max turns reached"), "{reason}");
        assert!(reason.contains("stderr: request failed"), "{reason}");
        assert!(!reason.contains(&fake_token), "{reason}");
    }
}
