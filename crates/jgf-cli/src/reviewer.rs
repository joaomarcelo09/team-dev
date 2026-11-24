//! The review step of an end-to-end run.
//!
//! A [`ReviewerRunner`] receives a read-only prompt (spec, task and the branch under review)
//! and answers [`ReviewDecision::Approved`] or [`ReviewDecision::Changes`]. The real runner
//! starts `claude -p` under bwrap with the same scratch home as the coder.
//! [`FakeReviewerRunner`] is the dry-run and test double.
//!
//! Filesystem access is isolated via bwrap: the host filesystem is not visible except for the
//! listed binds; the task work tree is writable except for its read-only `.git` directory, and
//! scratch is writable, while system directories and the single Claude credentials file are
//! read-only. `/tmp` is a fresh tmpfs, and `/proc` and `/dev` are fresh views. Network access
//! remains open, so the child can still reach whatever the network allows with the credentials it
//! has: Claude auth only. Full `jg-sandbox` integration is a follow-up.

use std::{
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

/// How often a running reviewer is checked for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Marker the reviewer prompt asks the model to print for an approval.
const APPROVED_MARKER: &str = "REVIEW: APPROVED";

/// One reviewer invocation.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRequest {
    /// State directory of the run; it is never mounted into the child.
    pub state_dir: PathBuf,
    /// Prompt sent to the reviewer; it asks for a read-only review.
    pub prompt: String,
    /// Work tree the reviewer reads.
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

impl ReviewRequest {
    /// The fixed argument vector used by the real runner and recorded in the audit log.
    pub fn command(&self) -> Vec<String> {
        vec![
            "claude".to_string(),
            "-p".to_string(),
            "--no-session-persistence".to_string(),
            "--max-turns".to_string(),
            self.max_turns.to_string(),
            "--max-budget-usd".to_string(),
            format!("{:.2}", self.max_budget_usd),
            self.prompt.clone(),
        ]
    }
}

/// What the reviewer decided about the pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReviewDecision {
    /// The changes are acceptable.
    Approved,
    /// The changes need one more fix round. This is the fail-closed default.
    #[default]
    Changes,
}

/// What one reviewer process did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReviewRun {
    /// Exit code when the process exited normally.
    pub exit_code: Option<i32>,
    /// Signal that ended the process, when it was killed.
    pub signal: Option<i32>,
    /// Whether the hard wall-clock limit killed the process.
    pub timed_out: bool,
    /// Whether the kill switch killed the process.
    pub killed: bool,
    /// Whether the reviewer stopped because its dollar budget ran out.
    pub budget_exhausted: bool,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Why the process could not be started, when it could not.
    pub spawn_error: Option<String>,
    /// Decision the reviewer reached.
    pub decision: ReviewDecision,
}

impl ReviewRun {
    /// Whether the reviewer exited successfully within its limits.
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
            format!("could not start the reviewer: {error}")
        } else if self.killed {
            "reviewer was killed by the kill switch".to_string()
        } else if self.timed_out {
            "reviewer hit its wall-clock limit".to_string()
        } else if self.budget_exhausted {
            "reviewer hit its dollar budget".to_string()
        } else if let Some(signal) = self.signal {
            format!("reviewer exited after signal {signal}")
        } else {
            let code = self
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "none".to_string());
            format!("reviewer exited with status {code}")
        };
        child::failure_reason(&summary, &self.stdout, &self.stderr)
    }
}

/// Runs one review request.
pub trait ReviewerRunner {
    /// Runs `request` and returns what the process decided.
    fn run(&self, request: &ReviewRequest) -> ReviewRun;

    /// Runs `request`, stopping and reaping the child as soon as `kill` is set.
    fn run_with_kill(&self, request: &ReviewRequest, kill: &dyn KillSwitch) -> ReviewRun {
        let _ = kill;
        self.run(request)
    }
}

/// The real reviewer: a read-only `claude -p` process in the work tree.
#[derive(Debug, Clone)]
pub struct ClaudeReviewerRunner {
    binary: PathBuf,
    bwrap_binary: PathBuf,
    real_home: Option<PathBuf>,
}

impl ClaudeReviewerRunner {
    /// A runner that starts the installed `claude` command.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            bwrap_binary: PathBuf::from(child::BWRAP_BINARY),
            real_home: None,
        }
    }

    /// A runner that starts `binary` and finds credentials under `home`; tests use both.
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

    fn scratch_home(&self, request: &ReviewRequest) -> Result<ScratchHome, ScratchError> {
        match &self.real_home {
            Some(home) => ScratchHome::prepare(&request.scratch_dir, home),
            None => ScratchHome::prepare(&request.scratch_dir, &crate::scratch::real_home()?),
        }
    }

    fn run_inner(&self, request: &ReviewRequest, kill: Option<&dyn KillSwitch>) -> ReviewRun {
        let argv = request.command();
        let started = Instant::now();

        let scratch = match self.scratch_home(request) {
            Ok(scratch) => scratch,
            Err(error) => {
                return ReviewRun {
                    spawn_error: Some(error.to_string()),
                    duration_ms: elapsed_ms(started),
                    ..ReviewRun::default()
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
                return ReviewRun {
                    spawn_error: Some(error),
                    duration_ms: elapsed_ms(started),
                    ..ReviewRun::default()
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
                return ReviewRun {
                    spawn_error: Some(format!(
                        "could not start bwrap at {}: {error}",
                        self.bwrap_binary.display()
                    )),
                    duration_ms: elapsed_ms(started),
                    ..ReviewRun::default()
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
                    return ReviewRun {
                        spawn_error: Some(error.to_string()),
                        duration_ms: elapsed_ms(started),
                        ..ReviewRun::default()
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
        let decision = if says_approved(&stdout) {
            ReviewDecision::Approved
        } else {
            ReviewDecision::Changes
        };

        ReviewRun {
            exit_code,
            signal,
            timed_out,
            killed,
            budget_exhausted,
            stdout,
            stderr,
            duration_ms: elapsed_ms(started),
            spawn_error,
            decision,
        }
    }
}

impl Default for ClaudeReviewerRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ReviewerRunner for ClaudeReviewerRunner {
    fn run(&self, request: &ReviewRequest) -> ReviewRun {
        self.run_inner(request, None)
    }

    fn run_with_kill(&self, request: &ReviewRequest, kill: &dyn KillSwitch) -> ReviewRun {
        self.run_inner(request, Some(kill))
    }
}

/// A [`ReviewerRunner`] with a fixed sequence of decisions, used by dry runs and tests.
#[derive(Debug, Default)]
pub struct FakeReviewerRunner {
    decisions: Mutex<std::collections::VecDeque<ReviewDecision>>,
    requests: Mutex<Vec<ReviewRequest>>,
}

impl FakeReviewerRunner {
    /// A runner that returns each decision in order, then keeps approving.
    pub fn new(decisions: impl IntoIterator<Item = ReviewDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// A runner that approves every review.
    pub fn approving() -> Self {
        Self::new([ReviewDecision::Approved])
    }

    /// Requests seen so far, in order.
    pub fn requests(&self) -> Vec<ReviewRequest> {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl ReviewerRunner for FakeReviewerRunner {
    fn run(&self, request: &ReviewRequest) -> ReviewRun {
        self.requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request.clone());
        let decision = self
            .decisions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or(ReviewDecision::Approved);
        ReviewRun {
            exit_code: Some(0),
            decision,
            ..ReviewRun::default()
        }
    }
}

/// Whether the reviewer output carries the approval marker.
fn says_approved(stdout: &str) -> bool {
    stdout.to_ascii_uppercase().contains(APPROVED_MARKER)
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
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
    };

    use super::*;

    fn request(dir: &Path) -> ReviewRequest {
        ReviewRequest {
            state_dir: dir.to_path_buf(),
            prompt: "review the plan".to_string(),
            work_dir: dir.to_path_buf(),
            scratch_dir: dir.join("scratch"),
            timeout: Duration::from_secs(30),
            max_turns: 5,
            max_budget_usd: 0.5,
        }
    }

    #[test]
    fn the_command_carries_the_limits_and_the_read_only_prompt() {
        let dir = std::env::temp_dir();
        let command = request(&dir).command();

        assert_eq!(command[0], "claude");
        assert_eq!(command[1], "-p");
        assert!(command.iter().any(|arg| arg == "--max-turns"));
        assert!(command.iter().any(|arg| arg == "5"));
        assert!(command.iter().any(|arg| arg == "--max-budget-usd"));
        assert!(command.iter().any(|arg| arg == "0.50"));
        assert_eq!(command.last().map(String::as_str), Some("review the plan"));
        assert!(
            !command
                .iter()
                .any(|arg| arg == "--dangerously-skip-permissions")
        );
    }

    #[test]
    fn the_fake_runner_returns_its_decisions_in_order() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let runner = FakeReviewerRunner::new([ReviewDecision::Changes, ReviewDecision::Approved]);

        assert_eq!(
            runner.run(&request(root.path())).decision,
            ReviewDecision::Changes
        );
        assert_eq!(
            runner.run(&request(root.path())).decision,
            ReviewDecision::Approved
        );
        assert_eq!(runner.requests().len(), 2);
    }

    #[test]
    fn an_unmarked_review_fails_closed_to_changes() {
        assert!(!says_approved("looks fine to me"));
        assert!(says_approved("All good.\nREVIEW: APPROVED"));
    }

    #[test]
    fn a_failed_reviewer_reason_includes_child_output() {
        let run = ReviewRun {
            exit_code: Some(1),
            stdout: "max turns reached".to_string(),
            stderr: "fake auth failure".to_string(),
            ..ReviewRun::default()
        };

        let reason = run.failure_reason();

        assert!(reason.contains("reviewer exited with status 1"), "{reason}");
        assert!(reason.contains("stdout: max turns reached"), "{reason}");
        assert!(reason.contains("stderr: fake auth failure"), "{reason}");
    }

    #[test]
    fn the_reviewer_child_sees_the_scratch_home() {
        let root = tempfile::TempDir::new().expect("temp dir");
        fs::create_dir(root.path().join(".git")).expect("work-tree git directory");
        let home = root.path().join("host-home");
        fs::create_dir_all(home.join(".claude")).expect("host claude dir");
        fs::write(
            home.join(".claude").join(".credentials.json"),
            b"placeholder",
        )
        .expect("fake credentials");
        let dump = root.path().join("reviewer.env");
        let script = root.path().join("fake-claude");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nenv > '{}'\necho REVIEW: APPROVED\n",
                dump.display()
            ),
        )
        .expect("write script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("make executable");
        let runner = ClaudeReviewerRunner::with_binary_and_home(&script, &home);

        let run = runner.run(&request(root.path()));

        assert!(run.succeeded(), "{:?}", run.failure_reason());
        assert_eq!(run.decision, ReviewDecision::Approved);
        let env = fs::read_to_string(&dump).expect("child env");
        let scratch_home: PathBuf = root.path().join("scratch").join("home");
        assert!(
            env.lines()
                .any(|line| line == format!("HOME={}", scratch_home.display())),
            "HOME is not the scratch home"
        );
        for key in ["GITHUB_TOKEN", "GH_TOKEN", "SSH_AUTH_SOCK", "GIT_ASKPASS"] {
            assert!(
                !env.lines().any(|line| line.starts_with(&format!("{key}="))),
                "{key} reached the reviewer"
            );
        }
    }
}
