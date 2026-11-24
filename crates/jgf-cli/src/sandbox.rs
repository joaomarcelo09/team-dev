//! The per-run jg-sandbox world for real Claude coder and reviewer children.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use jg_sandbox::{
    Backend, EgressEvent, EgressPolicy, EgressProxy, EgressSpec, ExecOutput, ExecRequest,
    NamespaceBackend, SandboxHandle, SandboxSpec, default_ro_binds,
};

use crate::{
    child::{self, RuntimeLayout},
    e2e::{KillSwitch, RunOutcome},
    lab_world::{EndOfLifePolicy, LabWorld, LabWorldSpec, RunEnd},
    scratch::{ScratchHome, real_home},
};

#[cfg(test)]
use crate::lab_world::CLAUDE_EGRESS_HOSTS;

const LAB_WORK_DIR: &str = "work";
const LAB_SCRATCH_DIR: &str = "scratch";
const EXEC_POLL_INTERVAL: Duration = Duration::from_millis(20);
const KILL_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const SCRATCH_EXCLUDE: &str = "/.jgf-claude-home/";

pub(crate) struct ClaudeSandbox {
    world: LabWorld,
    binary: PathBuf,
    backend: NamespaceBackend,
    live: Mutex<Option<LiveSandbox>>,
}

struct LiveSandbox {
    handle: SandboxHandle,
    proxy: Option<EgressProxy>,
    lab_dir: PathBuf,
    work_tree: PathBuf,
    scratch_root: PathBuf,
    credentials_file: PathBuf,
    runtime: RuntimeLayout,
}

#[derive(Debug, Default)]
pub(crate) struct ChildRun {
    pub output: Option<ExecOutput>,
    pub spawn_error: Option<String>,
    pub killed: bool,
}

impl ClaudeSandbox {
    #[cfg(test)]
    pub(crate) fn new(state_dir: impl Into<PathBuf>, binary: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        let audit_dir = state_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| state_dir.clone());
        let lab_id = state_dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("claude-lab")
            .to_string();
        Self::new_with_audit(
            state_dir,
            binary,
            audit_dir,
            lab_id,
            EndOfLifePolicy::KeepOnFailure,
        )
    }

    pub(crate) fn new_with_audit(
        state_dir: impl Into<PathBuf>,
        binary: impl Into<PathBuf>,
        audit_dir: impl Into<PathBuf>,
        lab_id: impl Into<String>,
        policy: EndOfLifePolicy,
    ) -> Self {
        Self {
            world: LabWorld::new(state_dir, audit_dir, lab_id, policy),
            binary: binary.into(),
            backend: NamespaceBackend::new(),
            live: Mutex::new(None),
        }
    }

    pub(crate) fn scratch_dir(lab_dir: &Path) -> PathBuf {
        lab_dir.join(LAB_SCRATCH_DIR)
    }

    pub(crate) fn finish(&self, outcome: &RunOutcome) -> Result<(), String> {
        let outcome = match outcome {
            RunOutcome::Delivered => RunEnd::Success,
            RunOutcome::Killed => RunEnd::Killed,
            RunOutcome::Failed { .. } | RunOutcome::BudgetExhausted => RunEnd::Failure,
        };
        self.world.finish(outcome, || self.destroy())
    }

    /// Creates the one isolated world used by both Claude workers in this run.
    pub(crate) fn create(&self, lab_dir: &Path, work_tree: &Path) -> Result<(), String> {
        let mut live = self
            .live
            .lock()
            .map_err(|_| "jg-sandbox state lock is poisoned".to_string())?;
        if live.is_some() {
            return Err("jg-sandbox world was already created for this run".to_string());
        }

        let lab_dir = fs::canonicalize(lab_dir).map_err(|error| {
            format!(
                "could not resolve Claude lab directory at {}: {error}",
                lab_dir.display()
            )
        })?;
        let work_tree = fs::canonicalize(work_tree).map_err(|error| {
            format!(
                "could not resolve Claude work tree at {}: {error}",
                work_tree.display()
            )
        })?;
        if work_tree != lab_dir.join(LAB_WORK_DIR) {
            return Err("Claude work tree must be the lab's work directory".to_string());
        }
        let scratch_root = Self::scratch_dir(&lab_dir);
        let created = self
            .world
            .create(&lab_dir, &work_tree, &scratch_root, |world_spec| {
                self.create_world(world_spec)
            })?;
        *live = Some(created);
        Ok(())
    }

    fn create_world(&self, world_spec: &LabWorldSpec) -> Result<LiveSandbox, String> {
        debug_assert_eq!(world_spec.uid, 1000);
        fs::create_dir_all(&world_spec.state_dir).map_err(|error| {
            format!(
                "could not create jg-sandbox state directory at {}: {error}",
                world_spec.state_dir.display()
            )
        })?;
        let state_dir = fs::canonicalize(&world_spec.state_dir).map_err(|error| {
            format!(
                "could not resolve jg-sandbox state directory at {}: {error}",
                world_spec.state_dir.display()
            )
        })?;
        child::validate_git_directory(&world_spec.work_tree)?;
        prepare_scratch_exclude(&world_spec.work_tree)?;
        let host_home = real_home().map_err(|error| error.to_string())?;
        let host_home = fs::canonicalize(&host_home).map_err(|error| {
            format!(
                "could not resolve Claude credential home at {}: {error}",
                host_home.display()
            )
        })?;
        let credentials_file = host_home.join(".claude/.credentials.json");
        match fs::metadata(&credentials_file) {
            Ok(metadata) if metadata.is_file() => {}
            _ => {
                return Err(format!(
                    "the real Claude credentials file was not found at {}",
                    credentials_file.display()
                ));
            }
        }
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&world_spec.scratch_home)
            .map_err(|error| {
                format!(
                    "could not create Claude scratch directory at {}: {error}",
                    world_spec.scratch_home.display()
                )
            })?;

        let created = (|| {
            let proxy_path = state_dir.join("egress.sock");
            let policy = EgressPolicy::with_ports(
                world_spec.egress_hosts.iter().copied(),
                world_spec.egress_ports.iter().copied(),
            )
            .map_err(|error| format!("invalid Claude egress allowlist: {error}"))?;
            let proxy = EgressProxy::start(&proxy_path, policy, Arc::new(log_egress))
                .map_err(|error| format!("could not start the jg-sandbox egress proxy: {error}"))?;
            let runtime = child::runtime_layout(
                &self.binary,
                &state_dir,
                &world_spec.work_tree,
                &world_spec.scratch_home,
                &host_home,
            )?;
            let spec = build_sandbox_spec(
                &state_dir,
                &world_spec.lab_dir,
                &world_spec.work_tree,
                &world_spec.read_only_git,
                &credentials_file,
                &proxy,
                &runtime,
            )?;
            let handle = self
                .backend
                .create(&spec)
                .map_err(|error| format!("could not create jg-sandbox world: {error}"))?;

            Ok(LiveSandbox {
                handle,
                proxy: Some(proxy),
                lab_dir: world_spec.lab_dir.clone(),
                work_tree: world_spec.work_tree.clone(),
                scratch_root: world_spec.scratch_home.clone(),
                credentials_file,
                runtime,
            })
        })();
        match created {
            Ok(world) => Ok(world),
            Err(error) => {
                let state_cleanup = fs::remove_dir(&state_dir);
                let scratch_cleanup = fs::remove_dir(&world_spec.scratch_home);
                if let Err(cleanup) = state_cleanup
                    && cleanup.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(format!(
                        "{error}; could not remove empty sandbox state: {cleanup}"
                    ));
                }
                if let Err(cleanup) = scratch_cleanup
                    && cleanup.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(format!(
                        "{error}; could not remove empty Claude scratch directory: {cleanup}"
                    ));
                }
                Err(error)
            }
        }
    }

    pub(crate) fn run_claude(
        &self,
        binary: &Path,
        argv: &[String],
        work_tree: &Path,
        scratch: &ScratchHome,
        timeout: Duration,
        kill: Option<&dyn KillSwitch>,
    ) -> ChildRun {
        let live_guard = match self.live.lock() {
            Ok(live_guard) => live_guard,
            Err(_) => {
                return ChildRun {
                    spawn_error: Some("jg-sandbox state lock is poisoned".to_string()),
                    ..ChildRun::default()
                };
            }
        };
        let Some(live) = live_guard.as_ref() else {
            return ChildRun {
                spawn_error: Some(
                    "jg-sandbox world is missing; refusing to run Claude unsandboxed".to_string(),
                ),
                ..ChildRun::default()
            };
        };
        if binary != self.binary {
            return ChildRun {
                spawn_error: Some(
                    "Claude executable does not match the jg-sandbox runtime configuration"
                        .to_string(),
                ),
                ..ChildRun::default()
            };
        }
        if fs::canonicalize(work_tree).ok().as_deref() != Some(live.work_tree.as_path()) {
            return ChildRun {
                spawn_error: Some(
                    "Claude work tree does not match the jg-sandbox lab directory".to_string(),
                ),
                ..ChildRun::default()
            };
        }
        if live.work_tree != live.lab_dir.join(LAB_WORK_DIR)
            || live.scratch_root != live.lab_dir.join(LAB_SCRATCH_DIR)
        {
            return ChildRun {
                spawn_error: Some(
                    "Claude work and scratch paths do not match the jg-sandbox lab".to_string(),
                ),
                ..ChildRun::default()
            };
        }
        if scratch.root() != live.scratch_root {
            return ChildRun {
                spawn_error: Some(
                    "Claude scratch home does not match the jg-sandbox scratch directory"
                        .to_string(),
                ),
                ..ChildRun::default()
            };
        }
        if argv.is_empty() {
            return ChildRun {
                spawn_error: Some("Claude argv must not be empty".to_string()),
                ..ChildRun::default()
            };
        }
        if kill.is_some_and(|kill| kill.killed()) {
            drop(live_guard);
            let destroy_error = self.world.finish(RunEnd::Killed, || self.destroy()).err();
            let mut result = ChildRun {
                killed: true,
                ..ChildRun::default()
            };
            if let Some(error) = destroy_error {
                result.spawn_error = Some(format!("could not stop jg-sandbox after kill: {error}"));
            }
            return result;
        }

        if scratch.credentials_file() != live.credentials_file {
            return ChildRun {
                spawn_error: Some(
                    "Claude credentials do not match the read-only jg-sandbox bind".to_string(),
                ),
                ..ChildRun::default()
            };
        }
        let request = match build_exec_request(
            argv,
            timeout,
            scratch,
            &live.lab_dir,
            &live.work_tree,
            &live.runtime,
        ) {
            Ok(request) => request,
            Err(error) => {
                return ChildRun {
                    spawn_error: Some(error),
                    ..ChildRun::default()
                };
            }
        };
        let handle = live.handle.clone();
        let backend = self.backend;
        drop(live_guard);
        if let Err(error) = self.world.start_run() {
            return ChildRun {
                spawn_error: Some(error),
                ..ChildRun::default()
            };
        }
        let _running = RunningWorld(&self.world);

        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = backend
                .exec(&handle, &request)
                .map_err(|error| format!("jg-sandbox exec failed: {error}"));
            let _ = sender.send(result);
        });

        loop {
            match receiver.recv_timeout(EXEC_POLL_INTERVAL) {
                Ok(Ok(output)) => {
                    return ChildRun {
                        output: Some(output),
                        ..ChildRun::default()
                    };
                }
                Ok(Err(error)) => {
                    return ChildRun {
                        spawn_error: Some(error),
                        ..ChildRun::default()
                    };
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return ChildRun {
                        spawn_error: Some(
                            "jg-sandbox exec worker exited without a result".to_string(),
                        ),
                        ..ChildRun::default()
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if kill.is_some_and(|kill| kill.killed()) {
                        let destroy_error =
                            self.world.finish(RunEnd::Killed, || self.destroy()).err();
                        let _ = receiver.recv_timeout(KILL_WAIT_TIMEOUT);
                        let mut result = ChildRun {
                            killed: true,
                            ..ChildRun::default()
                        };
                        if let Some(error) = destroy_error {
                            result.spawn_error =
                                Some(format!("could not stop jg-sandbox after kill: {error}"));
                        }
                        return result;
                    }
                }
            }
        }
    }

    /// Destroys the world and removes the private child home; safe to call more than once.
    pub(crate) fn destroy(&self) -> Result<(), String> {
        self.world.destroy(|| {
            let mut live = self
                .live
                .lock()
                .map_err(|_| "jg-sandbox state lock is poisoned".to_string())?;
            let Some(active) = live.as_mut() else {
                return Ok(());
            };
            let state_dir = active.handle.state_dir().to_path_buf();
            let result = self
                .backend
                .destroy(&active.handle)
                .map_err(|error| format!("could not destroy jg-sandbox world: {error}"));
            cleanup_after_destroy(result, &mut active.proxy, &state_dir, &active.scratch_root)
        })
    }
}

fn cleanup_after_destroy(
    result: Result<(), String>,
    proxy: &mut Option<EgressProxy>,
    state_dir: &Path,
    scratch_root: &Path,
) -> Result<(), String> {
    drop(proxy.take());

    let state_result = match fs::remove_dir(state_dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not remove jg-sandbox state directory at {}: {error}",
            state_dir.display()
        )),
    };
    let scratch_result = match fs::symlink_metadata(scratch_root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(scratch_root).map_err(|error| {
                format!(
                    "could not remove Claude scratch home at {}: {error}",
                    scratch_root.display()
                )
            })
        }
        Ok(_) => Err(format!(
            "refusing to remove unexpected Claude scratch path {}",
            scratch_root.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not inspect Claude scratch home at {}: {error}",
            scratch_root.display()
        )),
    };

    result.and(state_result).and(scratch_result)
}

struct RunningWorld<'a>(&'a LabWorld);

impl Drop for RunningWorld<'_> {
    fn drop(&mut self) {
        self.0.end_run();
    }
}

impl Drop for ClaudeSandbox {
    fn drop(&mut self) {
        let _ = self.world.destroy_if_not_kept(|| self.destroy());
    }
}

impl fmt::Debug for ClaudeSandbox {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClaudeSandbox")
            .field("state_dir", &self.world.state_dir())
            .field("binary", &self.binary)
            .finish_non_exhaustive()
    }
}

fn build_sandbox_spec(
    state_dir: &Path,
    lab_dir: &Path,
    work_tree: &Path,
    read_only_git: &Path,
    credentials_file: &Path,
    proxy: &EgressProxy,
    runtime: &RuntimeLayout,
) -> Result<SandboxSpec, String> {
    let lab_dir = fs::canonicalize(lab_dir)
        .map_err(|error| format!("could not resolve jg-sandbox lab directory: {error}"))?;
    let work_tree = fs::canonicalize(work_tree)
        .map_err(|error| format!("could not resolve Claude work tree: {error}"))?;
    if work_tree != lab_dir.join(LAB_WORK_DIR) {
        return Err("Claude work tree must be the lab's work directory".to_string());
    }
    let mut spec = SandboxSpec::new(state_dir, &lab_dir);
    let mut ro_binds = default_ro_binds();
    ro_binds.extend(runtime.directories.iter().cloned());
    ro_binds.push(credentials_file.to_path_buf());
    ro_binds.sort();
    ro_binds.dedup();
    spec.ro_binds = ro_binds;
    spec.lab_ro_binds.push(read_only_git.to_path_buf());
    spec.egress = Some(EgressSpec::new(proxy.socket_path()));
    spec.validate()
        .map_err(|error| format!("invalid Claude jg-sandbox spec: {error}"))?;
    Ok(spec)
}

fn build_exec_request(
    argv: &[String],
    timeout: Duration,
    scratch: &ScratchHome,
    lab_dir: &Path,
    work_tree: &Path,
    runtime: &RuntimeLayout,
) -> Result<ExecRequest, String> {
    let relative_work = work_tree
        .strip_prefix(lab_dir)
        .map_err(|_| "Claude work tree is outside the jg-sandbox lab".to_string())?;
    if relative_work != Path::new(LAB_WORK_DIR) {
        return Err("Claude work tree must be the lab's work directory".to_string());
    }
    let relative_scratch = scratch
        .root()
        .strip_prefix(lab_dir)
        .map_err(|_| "Claude scratch home is outside the jg-sandbox lab".to_string())?;
    if relative_scratch != Path::new(LAB_SCRATCH_DIR) {
        return Err("Claude scratch home must be the lab's scratch directory".to_string());
    }
    let guest_scratch = Path::new("/lab").join(relative_scratch);
    let guest_work = Path::new("/lab").join(relative_work);
    let mut child_argv = Vec::with_capacity(argv.len());
    child_argv.push(runtime.executable.to_string_lossy().into_owned());
    child_argv.extend(argv.iter().skip(1).cloned());

    let mut request = ExecRequest::new(child_argv)
        .with_cwd(guest_work)
        .with_timeout(timeout);
    request.env = scratch_environment(scratch, &guest_scratch, &runtime.path)?;
    Ok(request)
}

fn scratch_environment(
    scratch: &ScratchHome,
    guest_scratch: &Path,
    path: &std::ffi::OsStr,
) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    for (key, value) in scratch.environment() {
        let key = key
            .into_string()
            .map_err(|_| "Claude environment has a non-UTF-8 key".to_string())?;
        let value = match key.as_str() {
            "HOME" => guest_scratch.join("home").to_string_lossy().into_owned(),
            "XDG_CONFIG_HOME" => guest_scratch.join("config").to_string_lossy().into_owned(),
            "XDG_DATA_HOME" => guest_scratch.join("data").to_string_lossy().into_owned(),
            "XDG_CACHE_HOME" => guest_scratch.join("cache").to_string_lossy().into_owned(),
            "GH_CONFIG_DIR" => guest_scratch.join("gh").to_string_lossy().into_owned(),
            "CLAUDE_CONFIG_DIR" => guest_scratch.join("claude").to_string_lossy().into_owned(),
            "TMPDIR" => "/tmp".to_string(),
            "PATH" => path.to_string_lossy().into_owned(),
            _ => value.to_string_lossy().into_owned(),
        };
        env.insert(key, value);
    }
    Ok(env)
}

fn prepare_scratch_exclude(work_tree: &Path) -> Result<(), String> {
    let exclude = work_tree.join(".git/info/exclude");
    let metadata = fs::symlink_metadata(&exclude).map_err(|error| {
        format!(
            "could not inspect git exclude file at {}: {error}",
            exclude.display()
        )
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "git exclude file must be a plain file: {}",
            exclude.display()
        ));
    }
    let contents = fs::read_to_string(&exclude)
        .map_err(|error| format!("could not read git exclude file: {error}"))?;
    if contents.lines().any(|line| line.trim() == SCRATCH_EXCLUDE) {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .append(true)
        .open(&exclude)
        .map_err(|error| format!("could not open git exclude file: {error}"))?;
    if !contents.is_empty() && !contents.ends_with('\n') {
        file.write_all(b"\n")
            .map_err(|error| format!("could not update git exclude file: {error}"))?;
    }
    file.write_all(SCRATCH_EXCLUDE.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| format!("could not update git exclude file: {error}"))
}

fn log_egress(event: EgressEvent) {
    eprintln!(
        "jg-factory e2e egress {}:{} allowed={} ({})",
        event.host, event.port, event.allowed, event.reason
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_claude_spec_exposes_only_the_runtime_and_credentials_and_protects_git() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let work_tree = lab_dir.join(LAB_WORK_DIR);
        let git = work_tree.join(".git");
        let host_home = temp.path().join("host-home");
        fs::create_dir_all(&git).expect("git directory");
        fs::create_dir_all(host_home.join(".claude")).expect("claude directory");
        let credentials = host_home.join(".claude/.credentials.json");
        fs::write(&credentials, "placeholder").expect("placeholder credentials");
        let runtime_dir = temp.path().join("claude-runtime");
        fs::create_dir_all(&runtime_dir).expect("runtime directory");
        fs::write(runtime_dir.join("claude"), "placeholder").expect("fake executable");
        let state_dir = temp.path().join("state");
        let proxy_socket = temp.path().join("egress.sock");
        let policy = EgressPolicy::with_ports(CLAUDE_EGRESS_HOSTS, [443]).expect("policy");
        let proxy = EgressProxy::start(&proxy_socket, policy, Arc::new(|_| {})).expect("proxy");
        let runtime = RuntimeLayout {
            executable: runtime_dir.join("claude"),
            directories: vec![runtime_dir.clone()],
            path: runtime_dir.as_os_str().to_os_string(),
        };

        let spec = build_sandbox_spec(
            &state_dir,
            &lab_dir,
            &work_tree,
            &git,
            &credentials,
            &proxy,
            &runtime,
        )
        .expect("sandbox spec");

        assert_eq!(spec.lab_dir, lab_dir);
        assert_eq!(spec.lab_ro_binds, vec![git]);
        assert_eq!(spec.egress.as_ref().map(|egress| egress.port), Some(3128));
        assert!(spec.ro_binds.contains(&credentials));
        assert!(spec.ro_binds.contains(&runtime_dir));
        assert_eq!(
            spec.egress.as_ref().expect("egress").proxy_socket,
            proxy_socket
        );
        let policy = EgressPolicy::with_ports(CLAUDE_EGRESS_HOSTS, [443]).expect("policy");
        assert_eq!(policy.hosts(), &["api.anthropic.com"]);
        assert_eq!(policy.ports().iter().copied().collect::<Vec<_>>(), [443]);
        assert!(!policy.allows_host("github.com"));
        assert!(!policy.allows_host("api.github.com"));
    }

    #[test]
    fn exec_request_uses_the_guest_scratch_home_and_only_the_claude_command() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let work_tree = lab_dir.join(LAB_WORK_DIR);
        let scratch_root = ClaudeSandbox::scratch_dir(&lab_dir);
        let host_home = temp.path().join("host-home");
        fs::create_dir_all(&work_tree).expect("work tree");
        fs::create_dir_all(host_home.join(".claude")).expect("claude directory");
        fs::write(host_home.join(".claude/.credentials.json"), "placeholder")
            .expect("placeholder credentials");
        let scratch = ScratchHome::prepare(&scratch_root, &host_home).expect("scratch home");
        let runtime = RuntimeLayout {
            executable: PathBuf::from("/opt/claude-runtime/claude"),
            directories: vec![],
            path: "/usr/bin:/opt/claude-runtime".into(),
        };

        let request = build_exec_request(
            &["claude".to_string(), "-p".to_string()],
            Duration::from_secs(30),
            &scratch,
            &lab_dir,
            &work_tree,
            &runtime,
        )
        .expect("exec request");

        assert_eq!(
            request.argv,
            ["/opt/claude-runtime/claude".to_string(), "-p".to_string()]
        );
        assert_eq!(request.cwd, PathBuf::from("/lab/work"));
        assert_eq!(
            request.env.get("HOME").map(String::as_str),
            Some("/lab/scratch/home")
        );
        assert_eq!(
            request.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/lab/scratch/claude")
        );
        assert_eq!(
            request.env.get("GIT_CONFIG_GLOBAL").map(String::as_str),
            Some("/dev/null")
        );
        for key in ["GITHUB_TOKEN", "GH_TOKEN", "SSH_AUTH_SOCK", "GIT_ASKPASS"] {
            assert!(!request.env.contains_key(key), "{key} reached the child");
        }
    }

    #[test]
    fn a_missing_world_fails_closed_without_running_the_cli() {
        let temp = tempfile::tempdir().expect("temp dir");
        let sandbox = ClaudeSandbox::new(temp.path().join("state"), "claude");
        let host_home = temp.path().join("host-home");
        fs::create_dir_all(host_home.join(".claude")).expect("claude directory");
        fs::write(host_home.join(".claude/.credentials.json"), "placeholder")
            .expect("placeholder credentials");
        let lab_dir = temp.path().join("lab");
        let work_tree = lab_dir.join(LAB_WORK_DIR);
        fs::create_dir_all(&work_tree).expect("work tree");
        let scratch = ScratchHome::prepare(ClaudeSandbox::scratch_dir(&lab_dir), &host_home)
            .expect("scratch home");
        let request = sandbox.run_claude(
            Path::new("claude"),
            &["claude".to_string(), "-p".to_string()],
            &work_tree,
            &scratch,
            Duration::from_secs(1),
            None,
        );

        assert!(request.output.is_none());
        assert!(
            request
                .spawn_error
                .as_deref()
                .is_some_and(|error| error.contains("refusing to run Claude unsandboxed"))
        );
    }

    #[test]
    fn backend_destroy_failure_still_drops_proxy_and_removes_scratch_home() {
        let temp = tempfile::tempdir().expect("temp dir");
        let state_dir = temp.path().join("state");
        let scratch_root = temp.path().join("scratch");
        let socket_path = temp.path().join("egress.sock");
        fs::create_dir(&state_dir).expect("state directory");
        fs::create_dir_all(scratch_root.join("home")).expect("scratch home");
        let policy = EgressPolicy::with_ports(CLAUDE_EGRESS_HOSTS, [443]).expect("policy");
        let mut proxy =
            Some(EgressProxy::start(&socket_path, policy, Arc::new(|_| {})).expect("proxy"));

        let result = cleanup_after_destroy(
            Err("backend destroy failed".to_string()),
            &mut proxy,
            &state_dir,
            &scratch_root,
        );

        assert_eq!(result, Err("backend destroy failed".to_string()));
        assert!(proxy.is_none());
        assert!(!socket_path.exists());
        assert!(!state_dir.exists());
        assert!(!scratch_root.exists());
    }

    #[test]
    #[ignore = "requires the real Claude CLI, configured credentials, and one small API request"]
    fn real_claude_cli_runs_inside_jg_sandbox() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let work_tree = lab_dir.join(LAB_WORK_DIR);
        fs::create_dir_all(work_tree.join(".git/info")).expect("git info directory");
        fs::write(work_tree.join(".git/info/exclude"), "").expect("git exclude");

        let sandbox = Arc::new(ClaudeSandbox::new(temp.path().join("sandbox"), "claude"));
        sandbox
            .create(&lab_dir, &work_tree)
            .unwrap_or_else(|error| panic!("jg-sandbox setup failed: {error}"));
        let runner = crate::coder::ClaudeCoderRunner::in_sandbox(Arc::clone(&sandbox));
        let request = crate::coder::CoderRequest {
            state_dir: temp.path().join("state"),
            prompt: "Reply with exactly OK. Do not use tools.".to_string(),
            work_dir: work_tree.clone(),
            scratch_dir: ClaudeSandbox::scratch_dir(&lab_dir),
            timeout: Duration::from_secs(120),
            max_turns: 1,
            max_budget_usd: 0.50,
        };

        let run = crate::coder::CoderRunner::run(&runner, &request);

        assert!(run.succeeded(), "{}", run.failure_reason());
        assert!(run.stdout.trim().contains("OK"));
        sandbox.destroy().expect("destroy jg-sandbox world");
    }
}
