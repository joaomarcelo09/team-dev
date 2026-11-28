//! The per-run jg-sandbox world for real Claude coder and reviewer children.

use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use jg_sandbox::scope::{SCOPED_HOME, WORKSPACE};
use jg_sandbox::{
    Backend, EgressEvent, EgressPolicy, EgressProxy, EgressSpec, ExecOutput, ExecRequest,
    ExecScope, NamespaceBackend, SandboxHandle, SandboxSpec, ScopeAccess, default_ro_binds,
};
use jgf_core::{AuditKind, AuditLog, GithubGrant, ProjectId};
use jgf_hub::{
    Broker, BrokerShim, BrokerShimAuditEvent, GhCliBackend, HubShim, Role as HubRole, Router,
    Scheduler, ShimAuditEvent,
};

use crate::{
    child::{self, RuntimeLayout},
    e2e::{KillSwitch, RunOutcome},
    lab_world::{EndOfLifePolicy, LabWorld, LabWorldSpec, RunEnd, WorldStatus},
    roles::{BrokerSurface, MessageSurface, RoleExecution},
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
    world: Arc<LabWorld>,
    binary: PathBuf,
    backend: NamespaceBackend,
    live: Mutex<Option<LiveSandbox>>,
    broker_config: Option<BrokerRuntimeConfig>,
    project_root: PathBuf,
    router_dir: Option<PathBuf>,
    scheduler_dir: Option<PathBuf>,
}

struct LiveSandbox {
    handle: SandboxHandle,
    proxy: Option<EgressProxy>,
    hub: Option<HubShim>,
    broker: Option<BrokerShim<GhCliBackend, jgf_core::FileSecretStore>>,
    lab_dir: PathBuf,
    work_tree: PathBuf,
    font_dir: PathBuf,
    scratch_root: PathBuf,
    credentials_file: PathBuf,
    runtime: RuntimeLayout,
}

#[derive(Clone)]
pub(crate) struct BrokerRuntimeConfig {
    pub(crate) owner: String,
    pub(crate) broker_dir: PathBuf,
    pub(crate) audit_dir: PathBuf,
    pub(crate) lab: String,
    pub(crate) grant: GithubGrant,
    pub(crate) task_branch_prefix: String,
    pub(crate) backend_workdir: PathBuf,
    pub(crate) secret_dir: PathBuf,
}

struct ClaudeExec<'a> {
    binary: &'a Path,
    argv: &'a [String],
    work_tree: &'a Path,
    scratch: &'a ScratchHome,
    timeout: Duration,
    scope: Option<ExecScope>,
    expected_scope_access: Option<(ScopeAccess, Option<ScopeAccess>)>,
    message_environment: &'a BTreeMap<String, String>,
    kill: Option<&'a dyn KillSwitch>,
}

#[derive(Debug, Default)]
/// Result of one Claude command inside the sandbox.
pub struct ChildRun {
    /// Output returned by the sandbox backend, when the command ran.
    pub output: Option<ExecOutput>,
    /// Why the command could not start, if setup failed.
    pub spawn_error: Option<String>,
    /// Whether a kill switch stopped the command.
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
        let state_dir = state_dir.into();
        Self {
            world: Arc::new(LabWorld::new(state_dir.clone(), audit_dir, lab_id, policy)),
            binary: binary.into(),
            backend: NamespaceBackend::new(),
            live: Mutex::new(None),
            broker_config: None,
            project_root: state_dir,
            router_dir: None,
            scheduler_dir: None,
        }
    }

    pub(crate) fn with_broker(mut self, config: BrokerRuntimeConfig) -> Self {
        self.project_root = config.secret_dir.clone();
        self.broker_config = Some(config);
        self
    }

    pub(crate) fn with_router_dir(mut self, router_dir: impl Into<PathBuf>) -> Self {
        self.router_dir = Some(router_dir.into());
        self
    }

    pub(crate) fn with_scheduler_dir(mut self, scheduler_dir: impl Into<PathBuf>) -> Self {
        self.scheduler_dir = Some(scheduler_dir.into());
        self
    }

    pub(crate) fn role_project_root(&self) -> &Path {
        &self.project_root
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
        self.world.finish(outcome, || self.teardown())
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
        ensure_scratch_directory(&world_spec.scratch_home)?;

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
            let hub_socket_path = state_dir.join("hub.sock");
            let router_dir = self
                .router_dir
                .clone()
                .unwrap_or_else(|| state_dir.join("mail"));
            let router = Router::open(router_dir)
                .map_err(|error| format!("could not open lab message router: {error}"))?;
            let hub = if let Some(scheduler_dir) = &self.scheduler_dir {
                let scheduler = Scheduler::open(scheduler_dir)
                    .map_err(|error| format!("could not open lab scheduler: {error}"))?;
                let audit_world = Arc::clone(&self.world);
                HubShim::start_with_scheduler(
                    &hub_socket_path,
                    router,
                    scheduler,
                    move |event: ShimAuditEvent| {
                        let _ = audit_world.record_hub_event(&event);
                    },
                )
            } else {
                let audit_world = Arc::clone(&self.world);
                HubShim::start(&hub_socket_path, router, move |event: ShimAuditEvent| {
                    let _ = audit_world.record_hub_event(&event);
                })
            }
            .map_err(|error| format!("could not start the lab hub shim: {error}"))?;
            let broker = self.start_broker_shim(&state_dir)?;
            let spec = build_sandbox_spec(
                &state_dir,
                &world_spec.lab_dir,
                &world_spec.work_tree,
                &world_spec.font_dir,
                &world_spec.read_only_git,
                &credentials_file,
                &proxy,
                &runtime,
                broker.is_some(),
            )?;
            let handle = self
                .backend
                .create(&spec)
                .map_err(|error| format!("could not create jg-sandbox world: {error}"))?;

            Ok(LiveSandbox {
                handle,
                proxy: Some(proxy),
                hub: Some(hub),
                broker,
                lab_dir: world_spec.lab_dir.clone(),
                work_tree: world_spec.work_tree.clone(),
                font_dir: world_spec.font_dir.clone(),
                scratch_root: world_spec.scratch_home.clone(),
                credentials_file,
                runtime,
            })
        })();
        match created {
            Ok(world) => Ok(world),
            Err(error) => {
                let mail_cleanup = fs::remove_dir_all(state_dir.join("mail"));
                let state_cleanup = fs::remove_dir(&state_dir);
                let scratch_cleanup = fs::remove_dir(&world_spec.scratch_home);
                if let Err(cleanup) = mail_cleanup
                    && cleanup.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(format!(
                        "{error}; could not remove lab message store: {cleanup}"
                    ));
                }
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

    fn start_broker_shim(
        &self,
        state_dir: &Path,
    ) -> Result<Option<BrokerShim<GhCliBackend, jgf_core::FileSecretStore>>, String> {
        let Some(config) = self.broker_config.as_ref() else {
            return Ok(None);
        };
        let audit = AuditLog::open(&config.audit_dir, &config.lab)
            .map_err(|error| format!("could not open GitHub broker audit log: {error}"))?;
        let broker = Broker::open(&config.broker_dir, &config.lab, config.grant.clone(), audit)
            .map_err(|error| format!("could not open GitHub broker: {error}"))?
            .with_task_branch_prefix(&config.task_branch_prefix);
        let audit_dir = config.audit_dir.clone();
        let lab = config.lab.clone();
        let shim = BrokerShim::start(
            state_dir.join("broker.sock"),
            broker,
            GhCliBackend::new(&config.backend_workdir),
            jgf_core::FileSecretStore::new(&config.secret_dir),
            move |event: BrokerShimAuditEvent| {
                if let Ok(audit) = AuditLog::open(&audit_dir, &lab) {
                    let _ = audit.append(
                        None,
                        None,
                        AuditKind::Note {
                            text: event.audit_summary(),
                        },
                    );
                }
            },
        )
        .map_err(|error| format!("could not start the GitHub broker shim: {error}"))?;
        Ok(Some(shim))
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
        self.run_claude_with_scope(ClaudeExec {
            binary,
            argv,
            work_tree,
            scratch,
            timeout,
            scope: None,
            expected_scope_access: None,
            message_environment: &BTreeMap::new(),
            kill,
        })
    }

    fn run_claude_with_scope(&self, execution: ClaudeExec<'_>) -> ChildRun {
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
        if execution.binary != self.binary {
            return ChildRun {
                spawn_error: Some(
                    "Claude executable does not match the jg-sandbox runtime configuration"
                        .to_string(),
                ),
                ..ChildRun::default()
            };
        }
        let resolved_work_tree = fs::canonicalize(execution.work_tree).ok();
        let valid_work_tree = resolved_work_tree.as_ref().is_some_and(|work_tree| {
            if execution.scope.is_some() {
                work_tree.starts_with(&live.lab_dir) && work_tree != &live.lab_dir
            } else {
                work_tree == &live.work_tree
            }
        });
        if !valid_work_tree {
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
        let valid_scratch = if execution.scope.is_some() {
            execution.scratch.root().starts_with(&live.scratch_root)
                && execution.scratch.root() != live.scratch_root
                && !execution.scratch.root().starts_with(execution.work_tree)
                && !execution.work_tree.starts_with(execution.scratch.root())
        } else {
            execution.scratch.root() == live.scratch_root
        };
        if !valid_scratch {
            return ChildRun {
                spawn_error: Some(
                    "Claude scratch home does not match the jg-sandbox scratch directory"
                        .to_string(),
                ),
                ..ChildRun::default()
            };
        }
        if let Some(scope) = &execution.scope {
            let relative_work_tree = resolved_work_tree
                .as_ref()
                .and_then(|work_tree| work_tree.strip_prefix(&live.lab_dir).ok());
            let expected_scope_path = relative_work_tree.map(|path| Path::new("/lab").join(path));
            let relative_scratch = execution.scratch.root().strip_prefix(&live.lab_dir).ok();
            let expected_scratch_path = relative_scratch.map(|path| Path::new("/lab").join(path));
            let access_mismatch =
                execution
                    .expected_scope_access
                    .is_some_and(|(worktree, font)| {
                        scope.access != worktree || scope.font_access != font
                    });
            if expected_scope_path.as_ref() != Some(&scope.worktree)
                || expected_scratch_path.as_ref() != scope.scratch_home.as_ref()
                || access_mismatch
                || scope.validate().is_err()
            {
                return ChildRun {
                    spawn_error: Some(
                        "Claude exec scope does not match the jg-sandbox work tree".to_string(),
                    ),
                    ..ChildRun::default()
                };
            }
        }
        if execution.argv.is_empty() {
            return ChildRun {
                spawn_error: Some("Claude argv must not be empty".to_string()),
                ..ChildRun::default()
            };
        }
        if execution.kill.is_some_and(|kill| kill.killed()) {
            drop(live_guard);
            let destroy_error = self.world.finish(RunEnd::Killed, || self.teardown()).err();
            let mut result = ChildRun {
                killed: true,
                ..ChildRun::default()
            };
            if let Some(error) = destroy_error {
                result.spawn_error = Some(format!("could not stop jg-sandbox after kill: {error}"));
            }
            return result;
        }

        if execution.scratch.credentials_file() != live.credentials_file {
            return ChildRun {
                spawn_error: Some(
                    "Claude credentials do not match the read-only jg-sandbox bind".to_string(),
                ),
                ..ChildRun::default()
            };
        }
        let mut request = match build_exec_request_with_scope(
            execution.argv,
            execution.timeout,
            execution.scratch,
            &live.lab_dir,
            execution.work_tree,
            &live.runtime,
            execution.scope,
        ) {
            Ok(request) => request,
            Err(error) => {
                return ChildRun {
                    spawn_error: Some(error),
                    ..ChildRun::default()
                };
            }
        };
        request.env.extend(
            execution
                .message_environment
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
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
                    if execution.kill.is_some_and(|kill| kill.killed()) {
                        let destroy_error =
                            self.world.finish(RunEnd::Killed, || self.teardown()).err();
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

    pub(crate) fn role_lab_dir(&self) -> Result<PathBuf, String> {
        if self.world.status() == WorldStatus::Destroyed {
            return Err("jg-sandbox world is destroyed; refusing to launch a role".to_string());
        }
        let live = self
            .live
            .lock()
            .map_err(|_| "jg-sandbox state lock is poisoned".to_string())?;
        live.as_ref()
            .map(|live| live.lab_dir.clone())
            .ok_or_else(|| "jg-sandbox world is missing; refusing to launch a role".to_string())
    }

    pub(crate) fn role_font_dir(&self) -> Result<PathBuf, String> {
        if self.world.status() == WorldStatus::Destroyed {
            return Err("jg-sandbox world is destroyed; refusing to launch a role".to_string());
        }
        let live = self
            .live
            .lock()
            .map_err(|_| "jg-sandbox state lock is poisoned".to_string())?;
        live.as_ref()
            .map(|live| live.font_dir.clone())
            .ok_or_else(|| "jg-sandbox world is missing; refusing to launch a role".to_string())
    }

    pub(crate) fn audit_role_start(
        &self,
        project: &str,
        worker: &str,
        role: &str,
    ) -> Result<(), String> {
        self.world.record_role_start(project, worker, role)
    }

    pub(crate) fn register_hub_role(&self, role: HubRole) -> Result<MessageSurface, String> {
        let live = self
            .live
            .lock()
            .map_err(|_| "jg-sandbox state lock is poisoned".to_string())?;
        let hub = live
            .as_ref()
            .and_then(|active| active.hub.as_ref())
            .ok_or_else(|| "lab hub shim is unavailable".to_string())?;
        let token = hub
            .register_role(role)
            .map_err(|error| format!("could not register lab message identity: {error}"))?;
        Ok(MessageSurface::new(hub.socket_path().to_path_buf(), token))
    }

    pub(crate) fn revoke_hub_role(&self, surface: &MessageSurface) {
        if let Ok(live) = self.live.lock()
            && let Some(hub) = live.as_ref().and_then(|active| active.hub.as_ref())
        {
            hub.revoke(surface.token());
        }
    }

    pub(crate) fn register_github_broker_role(
        &self,
        project: &str,
        worker: &str,
    ) -> Result<BrokerSurface, String> {
        let owner = self
            .broker_config
            .as_ref()
            .map(|config| config.owner.as_str())
            .ok_or_else(|| "GitHub broker shim is unavailable".to_string())?;
        let project = ProjectId::parse(project).map_err(|error| error.to_string())?;
        let live = self
            .live
            .lock()
            .map_err(|_| "jg-sandbox state lock is poisoned".to_string())?;
        let broker = live
            .as_ref()
            .and_then(|active| active.broker.as_ref())
            .ok_or_else(|| "GitHub broker shim is unavailable".to_string())?;
        let token = broker
            .register_role(owner, project, worker)
            .map_err(|error| format!("could not register GitHub broker identity: {error}"))?;
        Ok(BrokerSurface::new(
            broker.socket_path().to_path_buf(),
            token,
        ))
    }

    pub(crate) fn revoke_github_broker_role(&self, surface: &BrokerSurface) {
        if let Ok(live) = self.live.lock()
            && let Some(broker) = live.as_ref().and_then(|active| active.broker.as_ref())
        {
            broker.revoke(surface.token());
        }
    }

    pub(crate) fn execute_role(
        &self,
        execution: RoleExecution,
        kill: Option<&dyn KillSwitch>,
    ) -> ChildRun {
        let host_home = match real_home() {
            Ok(home) => home,
            Err(error) => {
                return ChildRun {
                    spawn_error: Some(error.to_string()),
                    ..ChildRun::default()
                };
            }
        };
        let scratch = match ScratchHome::prepare(&execution.scratch_home, &host_home) {
            Ok(scratch) => scratch,
            Err(error) => {
                return ChildRun {
                    spawn_error: Some(error.to_string()),
                    ..ChildRun::default()
                };
            }
        };
        let mut message_environment: BTreeMap<String, String> = execution
            .project_environment
            .values()
            .iter()
            .map(|(key, secret)| (key.clone(), secret.expose().to_string()))
            .collect();
        message_environment.extend(execution.message_environment());
        message_environment.extend(execution.broker_environment());
        let expected_scope_access = Some((
            execution.role.worktree_access(),
            execution.role.font_access(),
        ));
        self.run_claude_with_scope(ClaudeExec {
            binary: &self.binary,
            argv: &execution.argv,
            work_tree: &execution.worktree,
            scratch: &scratch,
            timeout: execution.timeout,
            scope: Some(execution.scope),
            expected_scope_access,
            message_environment: &message_environment,
            kill,
        })
    }

    /// Destroys the world and removes the private child home; safe to call more than once.
    #[cfg(test)]
    pub(crate) fn destroy(&self) -> Result<(), String> {
        self.world.destroy(|| self.teardown())
    }

    /// Tears down the live sandbox and its host services.
    ///
    /// This is the closure handed to `LabWorld::destroy` and `LabWorld::finish`,
    /// which already hold the world's destroying state. It must not go through
    /// `destroy`: re-entering `LabWorld::destroy` fails with "lab world is
    /// already changing state" and leaves the sandbox init running.
    fn teardown(&self) -> Result<(), String> {
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
        cleanup_after_destroy(
            result,
            &mut active.hub,
            &mut active.broker,
            &mut active.proxy,
            &state_dir,
            &active.scratch_root,
        )
    }
}

fn cleanup_after_destroy(
    result: Result<(), String>,
    hub: &mut Option<HubShim>,
    broker: &mut Option<BrokerShim<GhCliBackend, jgf_core::FileSecretStore>>,
    proxy: &mut Option<EgressProxy>,
    state_dir: &Path,
    scratch_root: &Path,
) -> Result<(), String> {
    drop(hub.take());
    drop(broker.take());
    drop(proxy.take());

    let mail_result = match fs::remove_dir_all(state_dir.join("mail")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("could not remove lab message store: {error}")),
    };
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

    result
        .and(mail_result)
        .and(state_result)
        .and(scratch_result)
}

struct RunningWorld<'a>(&'a LabWorld);

impl Drop for RunningWorld<'_> {
    fn drop(&mut self) {
        self.0.end_run();
    }
}

impl Drop for ClaudeSandbox {
    fn drop(&mut self) {
        let _ = self.world.destroy_if_not_kept(|| self.teardown());
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

#[allow(clippy::too_many_arguments)]
fn build_sandbox_spec(
    state_dir: &Path,
    lab_dir: &Path,
    work_tree: &Path,
    font_dir: &Path,
    read_only_git: &Path,
    credentials_file: &Path,
    proxy: &EgressProxy,
    runtime: &RuntimeLayout,
    broker_enabled: bool,
) -> Result<SandboxSpec, String> {
    let lab_dir = fs::canonicalize(lab_dir)
        .map_err(|error| format!("could not resolve jg-sandbox lab directory: {error}"))?;
    let work_tree = fs::canonicalize(work_tree)
        .map_err(|error| format!("could not resolve Claude work tree: {error}"))?;
    let font_metadata = fs::symlink_metadata(font_dir)
        .map_err(|error| format!("could not inspect lab projects directory: {error}"))?;
    if !font_metadata.is_dir() || font_metadata.file_type().is_symlink() {
        return Err("lab projects directory must be a plain directory".to_string());
    }
    let font_dir = fs::canonicalize(font_dir)
        .map_err(|error| format!("could not resolve lab projects directory: {error}"))?;
    if work_tree != lab_dir.join(LAB_WORK_DIR) {
        return Err("Claude work tree must be the lab's work directory".to_string());
    }
    let mut spec = SandboxSpec::new(state_dir, &lab_dir);
    spec.font_dir = Some(font_dir);
    let mut ro_binds = default_ro_binds();
    ro_binds.extend(runtime.directories.iter().cloned());
    ro_binds.push(credentials_file.to_path_buf());
    ro_binds.sort();
    ro_binds.dedup();
    spec.ro_binds = ro_binds;
    spec.lab_ro_binds.push(read_only_git.to_path_buf());
    spec.allowed_sockets.push(state_dir.join("hub.sock"));
    if broker_enabled {
        spec.allowed_sockets.push(state_dir.join("broker.sock"));
    }
    spec.egress = Some(EgressSpec::new(proxy.socket_path()));
    spec.validate()
        .map_err(|error| format!("invalid Claude jg-sandbox spec: {error}"))?;
    Ok(spec)
}

fn build_exec_request_with_scope(
    argv: &[String],
    timeout: Duration,
    scratch: &ScratchHome,
    lab_dir: &Path,
    work_tree: &Path,
    runtime: &RuntimeLayout,
    scope: Option<ExecScope>,
) -> Result<ExecRequest, String> {
    let relative_work = work_tree
        .strip_prefix(lab_dir)
        .map_err(|_| "Claude work tree is outside the jg-sandbox lab".to_string())?;
    if relative_work.as_os_str().is_empty()
        || (scope.is_none() && relative_work != Path::new(LAB_WORK_DIR))
    {
        return Err("Claude work tree must be the lab's work directory".to_string());
    }
    let relative_scratch = scratch
        .root()
        .strip_prefix(lab_dir)
        .map_err(|_| "Claude scratch home is outside the jg-sandbox lab".to_string())?;
    if relative_scratch.as_os_str().is_empty()
        || (scope.is_none() && relative_scratch != Path::new(LAB_SCRATCH_DIR))
        || (scope.is_some() && !relative_scratch.starts_with(LAB_SCRATCH_DIR))
    {
        return Err("Claude scratch home must be the lab's scratch directory".to_string());
    }
    let guest_scratch = if scope.is_some() {
        PathBuf::from(SCOPED_HOME)
    } else {
        Path::new("/lab").join(relative_scratch)
    };
    let guest_work = Path::new("/lab").join(relative_work);
    let mut child_argv = Vec::with_capacity(argv.len());
    child_argv.push(runtime.executable.to_string_lossy().into_owned());
    child_argv.extend(argv.iter().skip(1).cloned());

    let mut request = ExecRequest::new(child_argv).with_timeout(timeout);
    request = match scope {
        Some(scope) => request.with_cwd(WORKSPACE).with_scope(scope),
        None => request.with_cwd(guest_work),
    };
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

fn ensure_scratch_directory(path: &Path) -> Result<(), String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|error| {
            format!(
                "could not create Claude scratch directory at {}: {error}",
                path.display()
            )
        })?;

    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "could not inspect Claude scratch directory at {}: {error}",
            path.display()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Claude scratch directory at {} must be a plain directory",
            path.display()
        ));
    }

    // SAFETY: geteuid has no preconditions and does not dereference memory.
    let current_uid = unsafe { libc::geteuid() };
    if metadata.uid() != current_uid || metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(format!(
            "Claude scratch directory at {} must be owned by the current user with mode 0700",
            path.display()
        ));
    }
    Ok(())
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
    use std::os::unix::{fs::symlink, net::UnixListener};

    #[test]
    fn the_claude_spec_exposes_only_the_runtime_and_credentials_and_protects_git() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let work_tree = lab_dir.join(LAB_WORK_DIR);
        let font_dir = temp.path().join("font");
        let git = work_tree.join(".git");
        let host_home = temp.path().join("host-home");
        fs::create_dir_all(&git).expect("git directory");
        fs::create_dir_all(&font_dir).expect("font directory");
        fs::create_dir_all(host_home.join(".claude")).expect("claude directory");
        let credentials = host_home.join(".claude/.credentials.json");
        fs::write(&credentials, "placeholder").expect("placeholder credentials");
        let runtime_dir = temp.path().join("claude-runtime");
        fs::create_dir_all(&runtime_dir).expect("runtime directory");
        fs::write(runtime_dir.join("claude"), "placeholder").expect("fake executable");
        let state_dir = temp.path().join("state");
        fs::create_dir_all(&state_dir).expect("state directory");
        let proxy_socket = temp.path().join("egress.sock");
        let policy = EgressPolicy::with_ports(CLAUDE_EGRESS_HOSTS, [443]).expect("policy");
        let proxy = EgressProxy::start(&proxy_socket, policy, Arc::new(|_| {})).expect("proxy");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let hub = HubShim::start(state_dir.join("hub.sock"), router, |_| {}).expect("hub");
        let runtime = RuntimeLayout {
            executable: runtime_dir.join("claude"),
            directories: vec![runtime_dir.clone()],
            path: runtime_dir.as_os_str().to_os_string(),
        };

        let spec = build_sandbox_spec(
            &state_dir,
            &lab_dir,
            &work_tree,
            &font_dir,
            &git,
            &credentials,
            &proxy,
            &runtime,
            false,
        )
        .expect("sandbox spec");

        assert_eq!(spec.lab_dir, lab_dir);
        assert_eq!(spec.font_dir.as_deref(), Some(font_dir.as_path()));
        assert_eq!(spec.lab_ro_binds, vec![git.clone()]);
        assert_eq!(spec.egress.as_ref().map(|egress| egress.port), Some(3128));
        assert!(spec.ro_binds.contains(&credentials));
        assert!(spec.ro_binds.contains(&runtime_dir));
        assert_eq!(spec.allowed_sockets, vec![hub.socket_path().to_path_buf()]);
        let broker_socket = state_dir.join("broker.sock");
        let _broker_listener = UnixListener::bind(&broker_socket).expect("broker socket");
        let broker_spec = build_sandbox_spec(
            &state_dir,
            &lab_dir,
            &work_tree,
            &font_dir,
            &git,
            &credentials,
            &proxy,
            &runtime,
            true,
        )
        .expect("sandbox spec with broker");
        assert_eq!(
            broker_spec.allowed_sockets,
            vec![hub.socket_path().to_path_buf(), broker_socket]
        );
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

        let request = build_exec_request_with_scope(
            &["claude".to_string(), "-p".to_string()],
            Duration::from_secs(30),
            &scratch,
            &lab_dir,
            &work_tree,
            &runtime,
            None,
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
    fn scoped_exec_request_mounts_only_the_selected_worktree_at_workspace() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let work_tree = lab_dir.join("projects/project-a/worktrees/task-1");
        let scratch_root = lab_dir.join("scratch/project-a/worker-1");
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
        let scope = ExecScope::new(
            jg_sandbox::ScopeAccess::ReadWrite,
            "/lab/projects/project-a/worktrees/task-1",
        )
        .with_common_dir("/lab/projects/project-a/repository/.git")
        .with_scratch_home("/lab/scratch/project-a/worker-1")
        .with_project("project-a");

        let request = build_exec_request_with_scope(
            &["claude".to_string(), "-p".to_string()],
            Duration::from_secs(30),
            &scratch,
            &lab_dir,
            &work_tree,
            &runtime,
            Some(scope.clone()),
        )
        .expect("scoped exec request");

        assert_eq!(request.cwd, PathBuf::from("/workspace"));
        assert_eq!(request.scope, Some(scope));
        assert_eq!(
            request.env.get("HOME").map(String::as_str),
            Some("/tmp/home/home")
        );
        let environment = request.resolved_env(Some(jg_sandbox::DEFAULT_EGRESS_PORT), None);
        for key in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            assert_eq!(
                environment.get(key).map(String::as_str),
                Some("http://127.0.0.1:3128"),
                "{key} must route role egress through the allowlist proxy"
            );
        }
        assert_eq!(
            environment.get("NO_PROXY").map(String::as_str),
            Some("localhost,127.0.0.1")
        );
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
    fn stale_claude_scratch_directory_is_reused_after_unclean_shutdown() {
        let temp = tempfile::tempdir().expect("temp dir");
        let scratch_root = temp.path().join("world/scratch");

        ensure_scratch_directory(&scratch_root).expect("initial scratch setup");
        let stale_file = scratch_root.join("lab-coordinator/home/left-behind");
        fs::create_dir_all(stale_file.parent().expect("scratch parent"))
            .expect("stale scratch directories");
        fs::write(&stale_file, "left behind by an unclean stop").expect("stale scratch file");

        ensure_scratch_directory(&scratch_root).expect("restart with existing scratch");

        assert_eq!(
            fs::read_to_string(stale_file).expect("stale file remains"),
            "left behind by an unclean stop"
        );
        let metadata = fs::symlink_metadata(&scratch_root).expect("scratch metadata");
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
    }

    #[test]
    fn scratch_directory_rejects_symlinks_and_unsafe_modes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let outside = temp.path().join("outside");
        let linked_scratch = temp.path().join("linked-scratch");
        fs::create_dir(&outside).expect("outside directory");
        symlink(&outside, &linked_scratch).expect("scratch symlink");

        assert!(ensure_scratch_directory(&linked_scratch).is_err());

        let unsafe_scratch = temp.path().join("unsafe-scratch");
        fs::create_dir(&unsafe_scratch).expect("unsafe scratch directory");
        fs::set_permissions(&unsafe_scratch, fs::Permissions::from_mode(0o755))
            .expect("unsafe scratch mode");
        assert!(ensure_scratch_directory(&unsafe_scratch).is_err());
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
        let mut hub = None;
        let mut broker = None;

        let result = cleanup_after_destroy(
            Err("backend destroy failed".to_string()),
            &mut hub,
            &mut broker,
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
    fn killed_finish_tears_down_the_live_world() {
        let has_credentials = real_home()
            .map(|home| home.join(".claude/.credentials.json").is_file())
            .unwrap_or(false);
        if !has_credentials {
            eprintln!("skipping: no Claude credentials file to bind into the sandbox");
            return;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let work_tree = lab_dir.join(LAB_WORK_DIR);
        fs::create_dir_all(work_tree.join(".git/info")).expect("git info directory");
        fs::write(work_tree.join(".git/info/exclude"), "").expect("git exclude");
        let state_dir = temp.path().join("sandbox");
        let sandbox = ClaudeSandbox::new(&state_dir, "claude");
        if let Err(error) = sandbox.create(&lab_dir, &work_tree) {
            eprintln!("skipping: jg-sandbox world unavailable: {error}");
            return;
        }
        let init_pid = {
            let live = sandbox.live.lock().expect("live lock");
            live.as_ref().expect("live world").handle.init_pid()
        };

        sandbox
            .finish(&RunOutcome::Killed)
            .expect("a killed run destroys its world");

        assert!(
            !state_dir.join("state.json").exists(),
            "sandbox state is removed"
        );
        assert!(
            !state_dir.join("hub.sock").exists(),
            "hub socket is removed"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while Path::new(&format!("/proc/{init_pid}")).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "sandbox init {init_pid} still runs after a killed finish"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        sandbox.destroy().expect("a second destroy is a no-op");
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
