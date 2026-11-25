//! Host-side supervisors for Lab and project Co-coordinator roles.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Write},
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread,
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use jg_sandbox::ExecOutput;
use jgf_core::ProjectId;
use jgf_hub::{
    Role as HubRole, Router, Scheduler, Watchdog,
    routing::{Message, RoutingError},
};
use thiserror::Error;

use crate::{
    e2e::KillSwitch,
    lab_runtime::{LabRuntime, LabRuntimeError, RoleLaunchService},
    lab_world::EndOfLifePolicy,
    roles::{Role, RoleExecution, RoleLaunch, RoleLaunchExecutor, RoleLauncher},
    sandbox::{ChildRun, ClaudeSandbox},
};

const COORDINATOR_DIR: &str = "coordinator";
const STATUS_FILE: &str = "status.json";
const SESSION_FILE: &str = "session";
const STOP_FILE: &str = "STOP";
const LOCK_FILE: &str = "supervisor.lock";
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
const CHILD_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF_SECS: u64 = 5;
const MAX_BACKOFF_SECS: u64 = 5 * 60;
const TURN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const TURN_MAX_TURNS: u32 = 10;
const TURN_MAX_BUDGET_USD: f64 = 0.5;
const COORDINATOR_PROJECT: &str = "lab-coordinator";
const COORDINATOR_WORKER: &str = "lab-coordinator";
const COORDINATOR_WORKTREE: &str = "main";
const CO_COORDINATOR_WORKER: &str = "co-coordinator";
const CO_COORDINATOR_WORKTREE: &str = "coordinator";

#[derive(Debug, Clone, PartialEq, Eq)]
enum CoordinatorRole {
    Lab,
    Project(String),
}

/// Supervisor inputs supplied by the CLI.
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Root containing routing, labs, and watchdog state.
    state_dir: PathBuf,
    /// Lab identifier.
    lab: String,
    /// Supervisor role.
    role: CoordinatorRole,
    /// Whether managed Co-coordinators should use the fake launcher.
    dry_run: bool,
}

impl CoordinatorConfig {
    /// Creates a configuration after validating the lab path segment.
    pub fn new(
        state_dir: impl Into<PathBuf>,
        lab: impl Into<String>,
    ) -> Result<Self, CoordinatorError> {
        let config = Self {
            state_dir: state_dir.into(),
            lab: lab.into(),
            role: CoordinatorRole::Lab,
            dry_run: false,
        };
        validate_lab(&config.lab)?;
        Ok(config)
    }

    /// Creates a configuration for one project's Co-coordinator.
    pub fn for_project(
        state_dir: impl Into<PathBuf>,
        lab: impl Into<String>,
        project: impl Into<String>,
    ) -> Result<Self, CoordinatorError> {
        let project = project.into();
        validate_project(&project)?;
        let config = Self {
            state_dir: state_dir.into(),
            lab: lab.into(),
            role: CoordinatorRole::Project(project),
            dry_run: false,
        };
        validate_lab(&config.lab)?;
        Ok(config)
    }

    /// Directory containing supervisor control and status files.
    pub fn coordinator_dir(&self) -> PathBuf {
        let lab_dir = self.state_dir.join("labs").join(&self.lab);
        match &self.role {
            CoordinatorRole::Lab => lab_dir.join(COORDINATOR_DIR),
            CoordinatorRole::Project(project) => lab_dir.join("coordinators").join(project),
        }
    }

    /// Root containing routing, labs, and watchdog state.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Lab identifier.
    pub fn lab(&self) -> &str {
        &self.lab
    }

    /// Project served by this Co-coordinator, or `None` for the Lab Coordinator.
    pub fn project(&self) -> Option<&str> {
        match &self.role {
            CoordinatorRole::Lab => None,
            CoordinatorRole::Project(project) => Some(project),
        }
    }
}

/// The current supervisor state written for the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorState {
    /// A coordinator turn is running.
    Running,
    /// The supervisor is waiting for a wake message.
    Idle,
    /// A turn failed and the supervisor is waiting before retrying.
    Backoff,
    /// The stop file ended the supervisor.
    Stopped,
}

impl CoordinatorState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Backoff => "backoff",
            Self::Stopped => "stopped",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "idle" => Some(Self::Idle),
            "backoff" => Some(Self::Backoff),
            "stopped" => Some(Self::Stopped),
            _ => None,
        }
    }
}

/// Publicly readable supervisor status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoordinatorStatus {
    /// Current supervisor state.
    pub state: CoordinatorState,
    /// Unix milliseconds when the last turn finished.
    pub last_turn_at_ms: Option<u64>,
    /// Safe diagnostic text; child output and secret values are never copied here.
    pub last_error: Option<String>,
    /// Remaining delay after a failed turn.
    pub backoff_secs: Option<u64>,
}

impl CoordinatorStatus {
    fn initial() -> Self {
        Self {
            state: CoordinatorState::Idle,
            last_turn_at_ms: None,
            last_error: None,
            backoff_secs: None,
        }
    }

    /// Reads the last status file; absent status means this supervisor is stopped.
    pub fn read(config: &CoordinatorConfig) -> Result<Self, CoordinatorError> {
        validate_lab(&config.lab)?;
        let path = config.coordinator_dir().join(STATUS_FILE);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Self {
                    state: CoordinatorState::Stopped,
                    ..Self::initial()
                });
            }
            Err(source) => return Err(io_error(&path, source)),
        };
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let state = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .and_then(CoordinatorState::parse)
            .ok_or_else(|| CoordinatorError::InvalidStatus(path.clone()))?;
        let last_turn_at_ms = value
            .get("last_turn_at_ms")
            .and_then(serde_json::Value::as_u64);
        let last_error = value
            .get("last_error")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let backoff_secs = value
            .get("backoff_secs")
            .and_then(serde_json::Value::as_u64);
        Ok(Self {
            state,
            last_turn_at_ms,
            last_error,
            backoff_secs,
        })
    }

    /// JSON form used by the CLI and desktop.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "status": self.state.as_str(),
            "last_turn_at_ms": self.last_turn_at_ms,
            "last_error": self.last_error.as_deref(),
            "backoff_secs": self.backoff_secs,
        })
        .to_string()
    }
}

/// Errors returned by coordinator supervisor operations.
#[derive(Debug, Error)]
pub enum CoordinatorError {
    /// Another supervisor already owns this role.
    #[error("a coordinator supervisor is already running for this role")]
    AlreadyRunning,
    /// The lab name is not a safe path segment.
    #[error("invalid lab identifier")]
    InvalidLab,
    /// The project name is not a safe path segment.
    #[error("invalid project identifier")]
    InvalidProject,
    /// The status file is not valid supervisor JSON.
    #[error("invalid coordinator status at {}", .0.display())]
    InvalidStatus(PathBuf),
    /// A filesystem operation failed.
    #[error("coordinator I/O error at {}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
    /// The routing mailbox could not be read or acknowledged.
    #[error(transparent)]
    Routing(#[from] RoutingError),
    /// The lab scheduler could not be opened or queried.
    #[error(transparent)]
    Scheduler(#[from] jgf_hub::SchedulerError),
    /// The watchdog heartbeat could not be written.
    #[error(transparent)]
    Watchdog(#[from] jgf_hub::WatchdogError),
    /// The lab runtime refused the coordinator turn.
    #[error(transparent)]
    Runtime(#[from] LabRuntimeError),
    /// A persisted JSON file could not be parsed or written.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// A real sandbox could not be prepared.
    #[error("coordinator sandbox setup failed: {0}")]
    Sandbox(String),
}

/// Cross-process exclusive lock held for the lifetime of a supervisor.
pub struct CoordinatorLock {
    file: File,
}

impl CoordinatorLock {
    /// Acquires this supervisor's lock without waiting.
    pub fn acquire(config: &CoordinatorConfig) -> Result<Self, CoordinatorError> {
        validate_lab(&config.lab)?;
        let dir = config.coordinator_dir();
        fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
        let path = dir.join(LOCK_FILE);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|source| io_error(&path, source))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == -1 {
            let source = io::Error::last_os_error();
            if source.kind() == ErrorKind::WouldBlock {
                return Err(CoordinatorError::AlreadyRunning);
            }
            return Err(io_error(&path, source));
        }
        Ok(Self { file })
    }
}

impl Drop for CoordinatorLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug, Clone)]
struct CoordinatorKillSwitch {
    path: PathBuf,
}

impl KillSwitch for CoordinatorKillSwitch {
    fn killed(&self) -> bool {
        self.path.exists()
    }
}

struct HeartbeatGuard {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<bool>>,
}

impl HeartbeatGuard {
    fn start(watchdog_dir: PathBuf) -> Result<Self, CoordinatorError> {
        let watchdog = Watchdog::open(watchdog_dir)?;
        let (stop, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            loop {
                match receiver.recv_timeout(CHILD_HEARTBEAT_INTERVAL) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return true,
                    Err(RecvTimeoutError::Timeout) => {
                        if watchdog.beat(now_secs()).is_err() {
                            return false;
                        }
                    }
                }
            }
        });
        Ok(Self {
            stop: Some(stop),
            worker: Some(worker),
        })
    }

    fn stop(&mut self) -> bool {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.worker
            .take()
            .is_some_and(|worker| worker.join().unwrap_or(false))
    }
}

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Creates the stop file observed by a running supervisor.
pub fn stop(config: &CoordinatorConfig) -> Result<PathBuf, CoordinatorError> {
    validate_lab(&config.lab)?;
    let dir = config.coordinator_dir();
    fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
    let path = dir.join(STOP_FILE);
    fs::write(&path, b"stop\n").map_err(|source| io_error(&path, source))?;
    Ok(path)
}

/// Runs the host-side supervisor using the supplied launch service.
pub fn run<L: RoleLaunchService>(
    config: &CoordinatorConfig,
    launcher: L,
) -> Result<(), CoordinatorError> {
    let lock = CoordinatorLock::acquire(config)?;
    run_locked(config, launcher, lock)
}

/// Runs the real or dry-run supervisor and selects a network-free fake launcher when requested.
pub fn run_cli(config: &CoordinatorConfig, dry_run: bool) -> Result<(), CoordinatorError> {
    let mut config = config.clone();
    config.dry_run = dry_run;
    let lock = CoordinatorLock::acquire(&config)?;
    if dry_run {
        let fake = DryRunRoleLaunchExecutor::new(&config)?;
        let skills = skills_dir();
        let launcher = RoleLauncher::new(skills, &fake);
        return run_locked(&config, launcher, lock);
    }

    let sandbox = prepare_real_sandbox(&config)?;
    let launcher = RoleLauncher::new(skills_dir(), &sandbox);
    let result = (|| {
        let world_dir = coordinator_world_dir(&config);
        sandbox
            .create(&world_dir, &world_dir.join("work"))
            .map_err(CoordinatorError::Sandbox)?;
        run_locked(&config, launcher, lock)
    })();
    let finish = sandbox.finish(&crate::e2e::RunOutcome::Killed);
    match (result, finish) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(CoordinatorError::Sandbox(error)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn run_locked<L: RoleLaunchService>(
    config: &CoordinatorConfig,
    launcher: L,
    _lock: CoordinatorLock,
) -> Result<(), CoordinatorError> {
    let dir = config.coordinator_dir();
    fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
    let stop_path = dir.join(STOP_FILE);
    match fs::remove_file(&stop_path) {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(source) => return Err(io_error(&stop_path, source)),
    }

    Supervisor::new(config, launcher)?.run()
}

struct Supervisor<L> {
    config: CoordinatorConfig,
    router: Router,
    runtime: LabRuntime<L>,
    watchdog: Watchdog,
    kill: CoordinatorKillSwitch,
    status: CoordinatorStatus,
    backoff_secs: u64,
    poll_interval: Duration,
    managed_coordinators: HashSet<String>,
    coordinator_processes: HashMap<String, Child>,
}

impl<L: RoleLaunchService> Supervisor<L> {
    fn new(config: &CoordinatorConfig, launcher: L) -> Result<Self, CoordinatorError> {
        validate_lab(&config.lab)?;
        let dir = config.coordinator_dir();
        fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
        let router = Router::open(config.state_dir.join("routing"))?;
        let scheduler = Scheduler::open(
            config
                .state_dir
                .join("labs")
                .join(&config.lab)
                .join("scheduler"),
        )?;
        let runtime = LabRuntime::new(scheduler, launcher, &config.state_dir, config.lab.clone())?;
        let watchdog_dir = match &config.role {
            CoordinatorRole::Lab => config
                .state_dir
                .join("labs")
                .join(&config.lab)
                .join("watchdog"),
            CoordinatorRole::Project(_) => dir.join("watchdog"),
        };
        let watchdog = Watchdog::open(watchdog_dir)?;
        Ok(Self {
            config: config.clone(),
            router,
            runtime,
            watchdog,
            kill: CoordinatorKillSwitch {
                path: dir.join(STOP_FILE),
            },
            status: CoordinatorStatus::initial(),
            backoff_secs: 0,
            poll_interval: DEFAULT_POLL_INTERVAL,
            managed_coordinators: HashSet::new(),
            coordinator_processes: HashMap::new(),
        })
    }

    fn run(mut self) -> Result<(), CoordinatorError> {
        self.write_status()?;
        loop {
            if self.kill.killed() {
                self.stop_managed_coordinators()?;
                self.status.state = CoordinatorState::Stopped;
                self.status.backoff_secs = None;
                self.write_status()?;
                return Ok(());
            }
            let delay = self.poll_once()?;
            if matches!(self.config.role, CoordinatorRole::Lab) {
                self.sync_coordinators()?;
            }
            self.wait_interruptibly(delay);
        }
    }

    fn poll_once(&mut self) -> Result<Duration, CoordinatorError> {
        let role = self.hub_role()?;
        let pending = match &self.config.role {
            CoordinatorRole::Lab => self.router.pending_wake(&role),
            CoordinatorRole::Project(_) => self
                .router
                .inbox(&role)
                .map(|messages| !messages.is_empty()),
        };
        if self.watchdog.beat(now_secs()).is_err() {
            return self.fail("could not write coordinator heartbeat");
        }
        let pending = match pending {
            Ok(pending) => pending,
            Err(_) => return self.fail("could not poll coordinator mailbox"),
        };
        if !pending {
            self.backoff_secs = 0;
            self.status.state = CoordinatorState::Idle;
            self.status.last_error = None;
            self.status.backoff_secs = None;
            self.write_status()?;
            return Ok(self.poll_interval);
        }

        let messages = match self.router.inbox(&role) {
            Ok(messages) => messages,
            Err(_) => return self.fail("could not read coordinator mailbox"),
        };
        let Some(last_message) = messages.last() else {
            self.status.state = CoordinatorState::Idle;
            self.status.last_error = None;
            self.status.backoff_secs = None;
            self.write_status()?;
            return Ok(self.poll_interval);
        };
        let through_id = last_message.id;
        let prompt = self.turn_prompt(&messages);
        let request = self.turn_request(prompt);
        let session = self.read_session();

        self.status.state = CoordinatorState::Running;
        self.status.last_error = None;
        self.status.backoff_secs = None;
        self.write_status()?;

        let mut heartbeat = match HeartbeatGuard::start(self.watchdog.dir().to_path_buf()) {
            Ok(heartbeat) => heartbeat,
            Err(_) => return self.fail("could not start coordinator heartbeat"),
        };
        let mut child = match self.launch_turn(&request, session.as_deref()) {
            Ok(child) => child,
            Err(_) => return self.fail("lab scheduler refused the coordinator turn"),
        };
        self.status.last_turn_at_ms = Some(now_millis());

        if session.is_some() && resume_failed(&child) {
            eprintln!("coordinator: resume failed; starting a fresh session");
            if self.clear_session().is_err() {
                return self.fail("could not clear the saved coordinator session");
            }
            child = match self.launch_turn(&request, None) {
                Ok(child) => child,
                Err(_) => return self.fail("lab scheduler refused the fresh coordinator turn"),
            };
            self.status.last_turn_at_ms = Some(now_millis());
        }

        if !heartbeat.stop() {
            return self.fail("could not refresh coordinator heartbeat");
        }
        if !turn_succeeded(&child) {
            return self.fail(turn_failure(&child));
        }

        if let Some(session_id) = extract_session_id(&child) {
            if self.write_session(&session_id).is_err() {
                return self.fail("could not save coordinator session");
            }
        } else {
            if self.clear_session().is_err() {
                return self.fail("could not clear the saved coordinator session");
            }
        }
        if self.router.ack_through(&role, through_id).is_err() {
            return self.fail("could not acknowledge coordinator messages");
        }

        self.backoff_secs = 0;
        self.status.state = CoordinatorState::Idle;
        self.status.last_error = None;
        self.status.backoff_secs = None;
        self.write_status()?;
        Ok(Duration::ZERO)
    }

    fn fail(&mut self, reason: &str) -> Result<Duration, CoordinatorError> {
        self.backoff_secs = next_backoff(self.backoff_secs);
        self.status.state = CoordinatorState::Backoff;
        self.status.last_error = Some(reason.to_string());
        self.status.backoff_secs = Some(self.backoff_secs);
        self.write_status()?;
        Ok(Duration::from_secs(self.backoff_secs))
    }

    fn wait_interruptibly(&self, delay: Duration) {
        let interval = Duration::from_millis(100);
        let mut remaining = delay;
        while !remaining.is_zero() && !self.kill.killed() {
            let step = remaining.min(interval);
            thread::sleep(step);
            remaining = remaining.saturating_sub(step);
        }
    }

    fn write_status(&self) -> Result<(), CoordinatorError> {
        let path = self.config.coordinator_dir().join(STATUS_FILE);
        let bytes = self.status.to_json();
        write_atomic(&path, bytes.as_bytes())
    }

    fn session_path(&self) -> PathBuf {
        self.config.coordinator_dir().join(SESSION_FILE)
    }

    fn read_session(&self) -> Option<String> {
        let path = self.session_path();
        let value = match fs::read_to_string(&path) {
            Ok(value) => value,
            Err(error) if error.kind() == ErrorKind::NotFound => return None,
            Err(_) => {
                eprintln!("coordinator: could not read saved session; starting fresh");
                return None;
            }
        };
        let value = value.trim();
        if valid_session_id(value) {
            Some(value.to_string())
        } else {
            eprintln!("coordinator: saved session was invalid; starting fresh");
            None
        }
    }

    fn write_session(&self, session_id: &str) -> Result<(), CoordinatorError> {
        let path = self.session_path();
        write_atomic(&path, session_id.as_bytes())
    }

    fn clear_session(&self) -> Result<(), CoordinatorError> {
        let path = self.session_path();
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_error(&path, source)),
        }
    }

    fn hub_role(&self) -> Result<HubRole, CoordinatorError> {
        match &self.config.role {
            CoordinatorRole::Lab => Ok(HubRole::LabCoordinator),
            CoordinatorRole::Project(project) => Ok(HubRole::CoCoordinator(
                ProjectId::parse(project).map_err(|_| CoordinatorError::InvalidProject)?,
            )),
        }
    }

    fn turn_prompt(&self, messages: &[Message]) -> String {
        match &self.config.role {
            CoordinatorRole::Lab => turn_prompt(messages),
            CoordinatorRole::Project(project) => co_coordinator_turn_prompt(project, messages),
        }
    }

    fn turn_request(&self, prompt: String) -> RoleLaunch {
        match &self.config.role {
            CoordinatorRole::Lab => coordinator_request(prompt),
            CoordinatorRole::Project(project) => co_coordinator_request(project, prompt),
        }
    }

    fn launch_turn(
        &self,
        request: &RoleLaunch,
        session: Option<&str>,
    ) -> Result<ChildRun, LabRuntimeError> {
        match &self.config.role {
            CoordinatorRole::Lab => {
                self.runtime
                    .launch_coordinator(request, session, Some(&self.kill))
            }
            CoordinatorRole::Project(project) => {
                self.runtime
                    .launch_co_coordinator(request, project, session, Some(&self.kill))
            }
        }
    }

    fn sync_coordinators(&mut self) -> Result<(), CoordinatorError> {
        self.refresh_coordinator_processes()?;
        let (start, stopping) =
            coordinator_reconciliation(self.runtime.scheduler(), &self.managed_coordinators)?;
        for project in stopping {
            let config = CoordinatorConfig::for_project(
                &self.config.state_dir,
                &self.config.lab,
                project.clone(),
            )?;
            stop(&config)?;
            if let Some(mut child) = self.coordinator_processes.remove(&project) {
                child
                    .wait()
                    .map_err(|source| io_error(Path::new("coordinator child"), source))?;
            }
            self.managed_coordinators.remove(&project);
        }
        for project in start {
            let config = CoordinatorConfig::for_project(
                &self.config.state_dir,
                &self.config.lab,
                project.clone(),
            )?;
            let mut config = config;
            config.dry_run = self.config.dry_run;
            if coordinator_is_running(&config)? {
                self.managed_coordinators.insert(project);
                continue;
            }
            let child = spawn_coordinator_process(&config)?;
            self.coordinator_processes.insert(project.clone(), child);
            self.managed_coordinators.insert(project);
        }
        Ok(())
    }

    fn refresh_coordinator_processes(&mut self) -> Result<(), CoordinatorError> {
        let mut exited = Vec::new();
        for (project, child) in &mut self.coordinator_processes {
            if child
                .try_wait()
                .map_err(|source| io_error(Path::new("coordinator child"), source))?
                .is_some()
            {
                exited.push(project.clone());
            }
        }
        for project in exited {
            self.coordinator_processes.remove(&project);
            self.managed_coordinators.remove(&project);
        }
        let external = self
            .managed_coordinators
            .iter()
            .filter(|project| !self.coordinator_processes.contains_key(*project))
            .cloned()
            .collect::<Vec<_>>();
        for project in external {
            let config = CoordinatorConfig::for_project(
                &self.config.state_dir,
                &self.config.lab,
                project.clone(),
            )?;
            if !coordinator_is_running(&config)? {
                self.managed_coordinators.remove(&project);
            }
        }
        Ok(())
    }

    fn stop_managed_coordinators(&mut self) -> Result<(), CoordinatorError> {
        for project in &self.managed_coordinators {
            let config = CoordinatorConfig::for_project(
                &self.config.state_dir,
                &self.config.lab,
                project.clone(),
            )?;
            stop(&config)?;
        }
        for (_, mut child) in self.coordinator_processes.drain() {
            child
                .wait()
                .map_err(|source| io_error(Path::new("coordinator child"), source))?;
        }
        self.managed_coordinators.clear();
        Ok(())
    }
}

/// Network-free executor selected by `coordinator run --dry-run`.
pub struct DryRunRoleLaunchExecutor {
    lab_dir: PathBuf,
    font_dir: PathBuf,
}

impl DryRunRoleLaunchExecutor {
    /// Creates the harmless local directories required by role preparation.
    pub fn new(config: &CoordinatorConfig) -> Result<Self, CoordinatorError> {
        validate_lab(&config.lab)?;
        let lab_dir = config.coordinator_dir().join("dry-run-world");
        let font_dir = config.state_dir.join("labs").join(&config.lab).join("font");
        fs::create_dir_all(&lab_dir).map_err(|source| io_error(&lab_dir, source))?;
        fs::create_dir_all(&font_dir).map_err(|source| io_error(&font_dir, source))?;
        Ok(Self { lab_dir, font_dir })
    }
}

impl RoleLaunchExecutor for DryRunRoleLaunchExecutor {
    fn lab_dir(&self) -> Result<PathBuf, String> {
        Ok(self.lab_dir.clone())
    }

    fn font_dir(&self) -> Result<PathBuf, String> {
        Ok(self.font_dir.clone())
    }

    fn record_role_start(&self, _project: &str, _worker: &str, _role: Role) -> Result<(), String> {
        Ok(())
    }

    fn run_role(&self, _execution: RoleExecution, kill: Option<&dyn KillSwitch>) -> ChildRun {
        if kill.is_some_and(KillSwitch::killed) {
            return ChildRun {
                killed: true,
                ..ChildRun::default()
            };
        }
        ChildRun {
            output: Some(ExecOutput {
                exit_code: Some(0),
                signal: None,
                stdout: br#"{"session_id":"dry-run-session"}"#.to_vec(),
                stderr: Vec::new(),
                timed_out: false,
                truncated: false,
            }),
            ..ChildRun::default()
        }
    }
}

/// Reads the status generated by the supervisor loop.
pub fn status(config: &CoordinatorConfig) -> Result<CoordinatorStatus, CoordinatorError> {
    CoordinatorStatus::read(config)
}

fn prepare_real_sandbox(config: &CoordinatorConfig) -> Result<ClaudeSandbox, CoordinatorError> {
    let coordinator_dir = config.coordinator_dir();
    let world_dir = coordinator_world_dir(config);
    let work_dir = world_dir.join("work");
    ensure_plain_directory(&world_dir)?;
    ensure_plain_directory(&work_dir)?;
    let git_dir = work_dir.join(".git");
    ensure_plain_directory(&git_dir)?;
    ensure_plain_directory(&git_dir.join("info"))?;
    let exclude = git_dir.join("info").join("exclude");
    match fs::symlink_metadata(&exclude) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(CoordinatorError::Sandbox(
                "coordinator git exclude must be a plain file".to_string(),
            ));
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::write(&exclude, "").map_err(|source| io_error(&exclude, source))?;
        }
        Err(source) => return Err(io_error(&exclude, source)),
    }
    let (project, worktree) = match &config.role {
        CoordinatorRole::Lab => (COORDINATOR_PROJECT, COORDINATOR_WORKTREE),
        CoordinatorRole::Project(project) => (project.as_str(), CO_COORDINATOR_WORKTREE),
    };
    let project_dir = world_dir.join("projects").join(project);
    let worktrees_dir = project_dir.join("worktrees");
    ensure_plain_directory(&world_dir.join("projects"))?;
    ensure_plain_directory(&project_dir)?;
    ensure_plain_directory(&worktrees_dir)?;
    let role_worktree = worktrees_dir.join(worktree);
    ensure_plain_directory(&role_worktree)?;
    if let CoordinatorRole::Project(project) = &config.role {
        let font_project = config
            .state_dir
            .join("labs")
            .join(&config.lab)
            .join("font")
            .join(project);
        ensure_plain_directory(&font_project)?;
        let spec = font_project.join("SPEC.md");
        match fs::symlink_metadata(&spec) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(CoordinatorError::Sandbox(
                    "project SPEC.md must be a plain file".to_string(),
                ));
            }
            Err(source) => return Err(io_error(&spec, source)),
        }
    }

    let sandbox = ClaudeSandbox::new_with_audit(
        coordinator_dir.join("sandbox-state"),
        "claude",
        &config.state_dir,
        config.lab.clone(),
        EndOfLifePolicy::KeepOnFailure,
    )
    .with_router_dir(config.state_dir.join("routing"))
    .with_scheduler_dir(
        config
            .state_dir
            .join("labs")
            .join(&config.lab)
            .join("scheduler"),
    );
    Ok(sandbox)
}

fn coordinator_world_dir(config: &CoordinatorConfig) -> PathBuf {
    config.coordinator_dir().join("world")
}

fn spawn_coordinator_process(config: &CoordinatorConfig) -> Result<Child, CoordinatorError> {
    let executable = std::env::current_exe()
        .map_err(|source| io_error(Path::new("current executable"), source))?;
    let mut command = Command::new(executable);
    command
        .arg("coordinator")
        .arg("run")
        .arg("--state-dir")
        .arg(&config.state_dir)
        .arg("--lab")
        .arg(&config.lab);
    if let Some(project) = config.project() {
        command.arg("--project").arg(project);
    }
    if config.dry_run {
        command.arg("--dry-run");
    }
    command
        .spawn()
        .map_err(|source| io_error(Path::new("coordinator child"), source))
}

fn coordinator_is_running(config: &CoordinatorConfig) -> Result<bool, CoordinatorError> {
    match CoordinatorLock::acquire(config) {
        Ok(lock) => {
            drop(lock);
            Ok(false)
        }
        Err(CoordinatorError::AlreadyRunning) => Ok(true),
        Err(error) => Err(error),
    }
}

fn coordinator_reconciliation(
    scheduler: &Scheduler,
    managed: &HashSet<String>,
) -> Result<(Vec<String>, Vec<String>), CoordinatorError> {
    let queued = scheduler
        .queued_projects()?
        .into_iter()
        .map(|project| project.as_str().to_string())
        .collect::<HashSet<_>>();
    let slots = scheduler
        .coder_slots()?
        .into_iter()
        .map(|slot| slot.project.as_str().to_string())
        .collect::<HashSet<_>>();

    let mut stop = managed
        .iter()
        .filter(|project| !queued.contains(*project) && !slots.contains(*project))
        .cloned()
        .collect::<Vec<_>>();
    stop.sort();

    let mut granted = slots;
    if let Some(project) = scheduler.next_to_start()? {
        granted.insert(project.as_str().to_string());
    }
    let mut start = granted
        .into_iter()
        .filter(|project| !managed.contains(project))
        .collect::<Vec<_>>();
    start.sort();
    Ok((start, stop))
}

fn skills_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("factory-skills")
}

fn coordinator_request(prompt: String) -> RoleLaunch {
    RoleLaunch {
        role: Role::LabCoordinator,
        project_id: COORDINATOR_PROJECT.to_string(),
        worker_id: COORDINATOR_WORKER.to_string(),
        worktree_name: COORDINATOR_WORKTREE.to_string(),
        prompt,
        timeout: TURN_TIMEOUT,
        max_turns: TURN_MAX_TURNS,
        max_budget_usd: TURN_MAX_BUDGET_USD,
    }
}

fn co_coordinator_request(project: &str, prompt: String) -> RoleLaunch {
    RoleLaunch {
        role: Role::CoCoordinator,
        project_id: project.to_string(),
        worker_id: CO_COORDINATOR_WORKER.to_string(),
        worktree_name: CO_COORDINATOR_WORKTREE.to_string(),
        prompt,
        timeout: TURN_TIMEOUT,
        max_turns: TURN_MAX_TURNS,
        max_budget_usd: TURN_MAX_BUDGET_USD,
    }
}

fn turn_prompt(messages: &[Message]) -> String {
    let mut prompt = String::from(
        "Process the following unread lab messages. Respond or take action according to your role instructions.\n\
         Available in-lab commands: `jg-factory mail inbox|next|ack`, `jg-factory mail send --to <role> --kind <kind> --body <text|@file>`, `jg-factory lab project create <name> --spec <file>`, and `jg-factory lab project queue <name>`. The host supervisor acknowledges successful Lab Coordinator turns.\n\n",
    );
    for message in messages {
        prompt.push_str(&format!(
            "## Message {} ({:?})\nFrom: {}\n",
            message.id, message.kind, message.from
        ));
        if let Some(project) = &message.project {
            prompt.push_str(&format!("Project: {project}\n"));
        }
        prompt.push_str(&format!(
            "Subject: {}\n\n{}\n\n",
            message.subject, message.body
        ));
    }
    prompt
}

fn co_coordinator_turn_prompt(project: &str, messages: &[Message]) -> String {
    let mut prompt = format!(
        "Process the following unread messages for project {project}. Read the project spec at `/font/{project}/SPEC.md` before planning. Respond or take action according to your role instructions. Available in-lab commands: `jg-factory mail next`, `jg-factory mail ack`, and `jg-factory mail send --to <role> --kind <kind> --body <text|@file>`. Send only Milestone, Failure, or BudgetEvent reports to `lab-coordinator`; routine worker results stay with you. The host supervisor acknowledges successful Co-coordinator turns.\n\n"
    );
    append_messages(&mut prompt, messages);
    prompt
}

fn append_messages(prompt: &mut String, messages: &[Message]) {
    for message in messages {
        prompt.push_str(&format!(
            "## Message {} ({:?})\nFrom: {}\n",
            message.id, message.kind, message.from
        ));
        if let Some(project) = &message.project {
            prompt.push_str(&format!("Project: {project}\n"));
        }
        prompt.push_str(&format!(
            "Subject: {}\n\n{}\n\n",
            message.subject, message.body
        ));
    }
}

fn turn_succeeded(child: &ChildRun) -> bool {
    child.spawn_error.is_none()
        && !child.killed
        && child
            .output
            .as_ref()
            .is_some_and(|output| output.exit_code == Some(0) && !output.timed_out)
}

fn resume_failed(child: &ChildRun) -> bool {
    if child.killed || child.output.as_ref().is_some_and(|output| output.timed_out) {
        return false;
    }
    if let Some(error) = child.spawn_error.as_deref() {
        return error.to_ascii_lowercase().contains("resume");
    }
    let Some(output) = child.output.as_ref() else {
        return false;
    };
    if output.exit_code == Some(0) {
        return false;
    }
    let diagnostic = format!(
        "{}\n{}",
        output.stdout_text().to_ascii_lowercase(),
        output.stderr_text().to_ascii_lowercase()
    );
    (diagnostic.contains("session") && diagnostic.contains("not found"))
        || diagnostic.contains("no conversation")
        || diagnostic.contains("invalid session")
        || diagnostic.contains("unable to resume")
        || diagnostic.contains("could not resume")
}

fn turn_failure(child: &ChildRun) -> &'static str {
    if child.killed {
        return "coordinator turn was stopped";
    }
    if child.output.as_ref().is_some_and(|output| output.timed_out) {
        return "coordinator turn timed out";
    }
    if child.spawn_error.is_some() {
        return "coordinator turn could not start";
    }
    "coordinator turn failed"
}

fn extract_session_id(child: &ChildRun) -> Option<String> {
    let output = child.output.as_ref()?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let session = value.get("session_id")?.as_str()?;
    valid_session_id(session).then(|| session.to_string())
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn next_backoff(current: u64) -> u64 {
    if current == 0 {
        INITIAL_BACKOFF_SECS
    } else {
        current.saturating_mul(2).min(MAX_BACKOFF_SECS)
    }
}

fn validate_lab(lab: &str) -> Result<(), CoordinatorError> {
    if lab.is_empty()
        || lab.len() > 64
        || !lab
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(CoordinatorError::InvalidLab);
    }
    Ok(())
}

fn validate_project(project: &str) -> Result<(), CoordinatorError> {
    ProjectId::parse(project)
        .map(|_| ())
        .map_err(|_| CoordinatorError::InvalidProject)
}

fn ensure_plain_directory(path: &Path) -> Result<(), CoordinatorError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(CoordinatorError::Sandbox(
            "coordinator directory must be a plain directory".to_string(),
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| io_error(path, source))
        }
        Err(source) => Err(io_error(path, source)),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CoordinatorError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .map_err(|source| io_error(&temp, source))?;
        file.write_all(bytes)
            .map_err(|source| io_error(&temp, source))?;
        file.sync_all().map_err(|source| io_error(&temp, source))?;
        fs::rename(&temp, path).map_err(|source| io_error(path, source))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_millis() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(u64::MAX)
}

fn io_error(path: &Path, source: io::Error) -> CoordinatorError {
    CoordinatorError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use jgf_hub::{MessageKind, OutgoingMessage, Role as HubRole, WorkerKind};

    use super::*;

    #[derive(Clone)]
    enum FakeTurn {
        Success(&'static str),
        Failure,
        ResumeFailure,
        Timeout,
        Stop,
    }

    struct FakeRoleLaunchExecutor {
        lab_dir: PathBuf,
        font_dir: PathBuf,
        turns: Mutex<VecDeque<FakeTurn>>,
        executions: Mutex<Vec<RoleExecution>>,
        stop_file: Mutex<Option<PathBuf>>,
        late_message_router: Mutex<Option<Router>>,
        status_file: Mutex<Option<PathBuf>>,
        running_status_seen: Mutex<bool>,
    }

    impl FakeRoleLaunchExecutor {
        fn new(root: &Path, turns: impl IntoIterator<Item = FakeTurn>) -> Self {
            let lab_dir = root.join("fake-lab");
            let font_dir = root.join("font");
            fs::create_dir_all(&lab_dir).expect("fake lab");
            fs::create_dir_all(&font_dir).expect("fake font");
            Self {
                lab_dir,
                font_dir,
                turns: Mutex::new(turns.into_iter().collect()),
                executions: Mutex::new(Vec::new()),
                stop_file: Mutex::new(None),
                late_message_router: Mutex::new(None),
                status_file: Mutex::new(None),
                running_status_seen: Mutex::new(false),
            }
        }

        fn stop_on_child(&self, path: PathBuf) {
            *self.stop_file.lock().expect("stop path lock") = Some(path);
        }

        fn send_message_on_child(&self, router: Router) {
            *self.late_message_router.lock().expect("late router lock") = Some(router);
        }

        fn observe_status(&self, path: PathBuf) {
            *self.status_file.lock().expect("status path lock") = Some(path);
        }
    }

    impl RoleLaunchExecutor for FakeRoleLaunchExecutor {
        fn lab_dir(&self) -> Result<PathBuf, String> {
            Ok(self.lab_dir.clone())
        }

        fn font_dir(&self) -> Result<PathBuf, String> {
            Ok(self.font_dir.clone())
        }

        fn record_role_start(
            &self,
            _project: &str,
            _worker: &str,
            _role: Role,
        ) -> Result<(), String> {
            Ok(())
        }

        fn project_environment(
            &self,
            _project_id: &str,
        ) -> Result<std::collections::BTreeMap<String, jgf_core::Secret>, String> {
            Ok(std::collections::BTreeMap::new())
        }

        fn run_role(&self, execution: RoleExecution, _kill: Option<&dyn KillSwitch>) -> ChildRun {
            if let Some(path) = self.status_file.lock().expect("status path lock").as_ref() {
                let status = fs::read(path).expect("running status exists");
                let value: serde_json::Value =
                    serde_json::from_slice(&status).expect("running status json");
                *self
                    .running_status_seen
                    .lock()
                    .expect("running status lock") =
                    value.get("status").and_then(serde_json::Value::as_str) == Some("running");
            }
            self.executions
                .lock()
                .expect("executions lock")
                .push(execution);
            if let Some(router) = self
                .late_message_router
                .lock()
                .expect("late router lock")
                .take()
            {
                router
                    .send(OutgoingMessage::new(
                        HubRole::User,
                        HubRole::LabCoordinator,
                        MessageKind::UserRequest,
                        "arrived during turn",
                        "keep this unread",
                    ))
                    .expect("late message");
            }
            match self.turns.lock().expect("turn queue").pop_front() {
                Some(FakeTurn::Success(session_id)) => successful_child(session_id),
                Some(FakeTurn::Failure) => failed_child(),
                Some(FakeTurn::ResumeFailure) => resume_failure_child(),
                Some(FakeTurn::Timeout) => timed_out_child(),
                Some(FakeTurn::Stop) => {
                    if let Some(path) = self.stop_file.lock().expect("stop path lock").as_ref() {
                        fs::write(path, "stop").expect("write stop file");
                    }
                    ChildRun {
                        killed: true,
                        ..ChildRun::default()
                    }
                }
                None => successful_child("fake-session"),
            }
        }
    }

    fn successful_child(session_id: &str) -> ChildRun {
        ChildRun {
            output: Some(ExecOutput {
                exit_code: Some(0),
                signal: None,
                stdout: format!(r#"{{"session_id":"{session_id}"}}"#).into_bytes(),
                stderr: Vec::new(),
                timed_out: false,
                truncated: false,
            }),
            ..ChildRun::default()
        }
    }

    fn failed_child() -> ChildRun {
        ChildRun {
            output: Some(ExecOutput {
                exit_code: Some(1),
                signal: None,
                stdout: b"{}".to_vec(),
                stderr: b"do not copy child output into status".to_vec(),
                timed_out: false,
                truncated: false,
            }),
            ..ChildRun::default()
        }
    }

    fn resume_failure_child() -> ChildRun {
        ChildRun {
            output: Some(ExecOutput {
                exit_code: Some(1),
                signal: None,
                stdout: Vec::new(),
                stderr: b"Session not found".to_vec(),
                timed_out: false,
                truncated: false,
            }),
            ..ChildRun::default()
        }
    }

    fn timed_out_child() -> ChildRun {
        ChildRun {
            output: Some(ExecOutput {
                exit_code: None,
                signal: Some(libc::SIGKILL),
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
                truncated: false,
            }),
            ..ChildRun::default()
        }
    }

    fn make_config(root: &Path) -> CoordinatorConfig {
        CoordinatorConfig::new(root, "alpha").expect("config")
    }

    #[test]
    fn supervisor_prompt_lists_in_lab_commands() {
        let prompt = turn_prompt(&[]);

        assert!(prompt.contains("jg-factory mail inbox|next|ack"));
        assert!(prompt.contains("jg-factory mail send --to <role> --kind <kind>"));
        assert!(prompt.contains("jg-factory lab project create <name> --spec <file>"));
        assert!(prompt.contains("jg-factory lab project queue <name>"));
    }

    fn add_message(config: &CoordinatorConfig) -> Message {
        let router = Router::open(config.state_dir.join("routing")).expect("router");
        router
            .send(OutgoingMessage::new(
                HubRole::User,
                HubRole::LabCoordinator,
                MessageKind::UserRequest,
                "please help",
                "Do the task",
            ))
            .expect("message")
    }

    fn add_project_message(
        config: &CoordinatorConfig,
        project: &str,
        kind: MessageKind,
    ) -> Message {
        let project_id = ProjectId::parse(project).expect("project id");
        let router = Router::open(config.state_dir.join("routing")).expect("router");
        router
            .send(OutgoingMessage::new(
                HubRole::LabCoordinator,
                HubRole::CoCoordinator(project_id),
                kind,
                "project work",
                "Read the spec and prepare the project.",
            ))
            .expect("project message")
    }

    fn supervisor<'a>(
        config: &CoordinatorConfig,
        executor: &'a FakeRoleLaunchExecutor,
    ) -> Supervisor<RoleLauncher<'a>> {
        let launcher = RoleLauncher::new(skills_dir(), executor);
        Supervisor::new(config, launcher).expect("supervisor")
    }

    fn run_single_poll<L: RoleLaunchService>(supervisor: &mut Supervisor<L>) -> Duration {
        supervisor.poll_once().expect("poll")
    }

    #[test]
    fn co_coordinator_turn_uses_its_project_mailbox_scope_and_spec() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = CoordinatorConfig::for_project(root.path(), "alpha", "project-a")
            .expect("project config");
        let scheduler = Scheduler::open(
            config
                .state_dir
                .join("labs")
                .join(&config.lab)
                .join("scheduler"),
        )
        .expect("scheduler");
        scheduler
            .enqueue(ProjectId::parse("project-a").expect("project"))
            .expect("queue project");
        let font_project = root.path().join("font/project-a");
        fs::create_dir_all(&font_project).expect("font project");
        fs::write(font_project.join("SPEC.md"), "# project spec").expect("spec");
        let router = Router::open(config.state_dir.join("routing")).expect("router");
        add_project_message(&config, "project-a", MessageKind::Demand);
        let worker = HubRole::worker(
            ProjectId::parse("project-a").expect("project"),
            "coder-1",
            WorkerKind::Coder,
        )
        .expect("worker role");
        router
            .send(OutgoingMessage::new(
                worker,
                HubRole::CoCoordinator(ProjectId::parse("project-a").expect("project")),
                MessageKind::WorkerResult,
                "task complete",
                "Routine worker result.",
            ))
            .expect("worker result");
        let executor =
            FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Success("project-session")]);
        let mut project_supervisor = supervisor(&config, &executor);

        assert_eq!(run_single_poll(&mut project_supervisor), Duration::ZERO);

        let executions = executor.executions.lock().expect("executions");
        assert_eq!(executions.len(), 1);
        let execution = &executions[0];
        assert_eq!(execution.role, Role::CoCoordinator);
        assert_eq!(execution.project_id, "project-a");
        assert_eq!(
            execution.scope.font_access,
            Some(jg_sandbox::ScopeAccess::ReadWrite)
        );
        assert_eq!(execution.scope.font_project.as_deref(), Some("project-a"));
        assert!(
            execution
                .argv
                .last()
                .is_some_and(|prompt| prompt.contains("/font/project-a/SPEC.md"))
        );
        assert!(
            execution
                .argv
                .last()
                .is_some_and(|prompt| prompt.contains("Routine worker result."))
        );
        assert_eq!(
            fs::read_to_string(config.coordinator_dir().join(SESSION_FILE))
                .expect("project session"),
            "project-session"
        );
        assert!(
            router
                .inbox(&HubRole::CoCoordinator(
                    ProjectId::parse("project-a").expect("project")
                ))
                .expect("coordinator inbox")
                .is_empty()
        );
        assert!(
            router
                .inbox(&HubRole::LabCoordinator)
                .expect("lab coordinator inbox")
                .is_empty()
        );
    }

    #[test]
    fn coordinator_manager_only_starts_granted_projects_and_stops_finished_ones() {
        let root = tempfile::tempdir().expect("tempdir");
        let lab_config = make_config(root.path());
        let scheduler = Scheduler::open(
            lab_config
                .state_dir
                .join("labs")
                .join(&lab_config.lab)
                .join("scheduler"),
        )
        .expect("scheduler");
        for name in ["project-a", "project-b"] {
            scheduler
                .enqueue(ProjectId::parse(name).expect("project"))
                .expect("queue project");
        }

        let managed = HashSet::new();
        let (start, stopping) =
            coordinator_reconciliation(&scheduler, &managed).expect("reconcile");
        assert_eq!(start, vec!["project-a"]);
        assert!(stopping.is_empty());

        let config = CoordinatorConfig::for_project(root.path(), "alpha", "project-a")
            .expect("project config");
        add_project_message(&config, "project-a", MessageKind::Demand);
        let executor = FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Success("session-a")]);
        let mut project_supervisor = supervisor(&config, &executor);
        assert_eq!(run_single_poll(&mut project_supervisor), Duration::ZERO);
        assert_eq!(executor.executions.lock().expect("turns").len(), 1);
        assert!(
            !root
                .path()
                .join("labs/alpha/coordinators/project-b/status.json")
                .exists()
        );

        let managed = HashSet::from(["project-a".to_string()]);
        scheduler
            .remove(&ProjectId::parse("project-a").expect("project"))
            .expect("finish project");
        let (start, stopping) =
            coordinator_reconciliation(&scheduler, &managed).expect("finish reconcile");
        assert_eq!(start, vec!["project-b"]);
        assert_eq!(stopping, vec!["project-a"]);
        let finished = CoordinatorConfig::for_project(root.path(), "alpha", "project-a")
            .expect("finished config");
        stop(&finished).expect("stop finished project");
        assert!(finished.coordinator_dir().join(STOP_FILE).is_file());
    }

    #[test]
    fn project_supervisor_locks_and_status_files_are_isolated() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = CoordinatorConfig::for_project(root.path(), "alpha", "project-a")
            .expect("first config");
        let second = CoordinatorConfig::for_project(root.path(), "alpha", "project-b")
            .expect("second config");
        let _first_lock = CoordinatorLock::acquire(&first).expect("first lock");
        let _second_lock = CoordinatorLock::acquire(&second).expect("second lock");
        assert_ne!(first.coordinator_dir(), second.coordinator_dir());
        fs::create_dir_all(first.coordinator_dir()).expect("first state");
        fs::create_dir_all(second.coordinator_dir()).expect("second state");
        write_atomic(
            &first.coordinator_dir().join(STATUS_FILE),
            br#"{"status":"running"}"#,
        )
        .expect("first status");
        write_atomic(
            &second.coordinator_dir().join(STATUS_FILE),
            br#"{"status":"backoff"}"#,
        )
        .expect("second status");
        fs::write(first.coordinator_dir().join(SESSION_FILE), "first-session")
            .expect("first session");
        fs::write(
            second.coordinator_dir().join(SESSION_FILE),
            "second-session",
        )
        .expect("second session");

        assert_eq!(
            CoordinatorStatus::read(&first).expect("first status").state,
            CoordinatorState::Running
        );
        assert_eq!(
            CoordinatorStatus::read(&second)
                .expect("second status")
                .state,
            CoordinatorState::Backoff
        );
        assert_eq!(
            fs::read_to_string(first.coordinator_dir().join(SESSION_FILE)).expect("first session"),
            "first-session"
        );
        assert_eq!(
            fs::read_to_string(second.coordinator_dir().join(SESSION_FILE))
                .expect("second session"),
            "second-session"
        );
    }

    #[test]
    fn unread_message_launches_one_turn_then_acks_only_the_snapshot() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        add_message(&config);
        let executor = FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Success("session-1")]);
        let mut supervisor = supervisor(&config, &executor);

        let delay = run_single_poll(&mut supervisor);

        assert_eq!(delay, Duration::ZERO);
        assert_eq!(
            Router::open(config.state_dir.join("routing"))
                .expect("router")
                .inbox(&HubRole::LabCoordinator)
                .expect("inbox")
                .len(),
            0
        );
        let executions = executor.executions.lock().expect("executions");
        assert_eq!(executions.len(), 1);
        let execution = &executions[0];
        assert_eq!(execution.role, Role::LabCoordinator);
        assert_eq!(execution.timeout, TURN_TIMEOUT);
        assert_eq!(execution.argv.get(1).map(String::as_str), Some("-p"));
        assert!(execution.argv.contains(&"--output-format".to_string()));
        assert!(
            execution
                .argv
                .last()
                .is_some_and(|prompt| prompt.contains("Do the task"))
        );
        assert_eq!(
            fs::read_to_string(config.coordinator_dir().join(SESSION_FILE)).expect("session"),
            "session-1"
        );
        assert_eq!(
            CoordinatorStatus::read(&config).expect("status").state,
            CoordinatorState::Idle
        );
        assert!(supervisor.status.last_turn_at_ms.is_some());
        assert!(
            Watchdog::open(config.state_dir.join("labs/alpha/watchdog"))
                .expect("watchdog")
                .last_beat()
                .expect("heartbeat")
                .is_some()
        );
    }

    #[test]
    fn turn_acknowledges_only_messages_in_its_prompt_snapshot() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        add_message(&config);
        let router = Router::open(config.state_dir.join("routing")).expect("router");
        let executor = FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Success("session")]);
        executor.send_message_on_child(router.clone());
        let mut supervisor = supervisor(&config, &executor);

        assert_eq!(run_single_poll(&mut supervisor), Duration::ZERO);

        let unread = router.inbox(&HubRole::LabCoordinator).expect("inbox");
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].subject, "arrived during turn");
    }

    #[test]
    fn failed_turn_is_not_acked_and_status_uses_growing_capped_backoff() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        add_message(&config);
        let executor = FakeRoleLaunchExecutor::new(
            root.path(),
            [FakeTurn::Failure, FakeTurn::Failure, FakeTurn::Failure],
        );
        let mut supervisor = supervisor(&config, &executor);

        assert_eq!(run_single_poll(&mut supervisor), Duration::from_secs(5));
        assert_eq!(run_single_poll(&mut supervisor), Duration::from_secs(10));
        assert_eq!(run_single_poll(&mut supervisor), Duration::from_secs(20));
        assert_eq!(
            Router::open(config.state_dir.join("routing"))
                .expect("router")
                .inbox(&HubRole::LabCoordinator)
                .expect("inbox")
                .len(),
            1
        );
        let status = CoordinatorStatus::read(&config).expect("status");
        assert_eq!(status.state, CoordinatorState::Backoff);
        assert_eq!(status.backoff_secs, Some(20));
        assert_eq!(
            status.last_error.as_deref(),
            Some("coordinator turn failed")
        );
        assert!(!status.to_json().contains("do not copy child output"));

        let mut delay = 20;
        for _ in 0..10 {
            delay = next_backoff(delay);
        }
        assert_eq!(delay, MAX_BACKOFF_SECS);
        assert_eq!(next_backoff(MAX_BACKOFF_SECS), MAX_BACKOFF_SECS);
    }

    #[test]
    fn a_failed_resume_starts_fresh_and_saves_the_new_session() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        add_message(&config);
        fs::create_dir_all(config.coordinator_dir()).expect("coordinator directory");
        fs::write(config.coordinator_dir().join(SESSION_FILE), "old-session").expect("old session");
        let executor = FakeRoleLaunchExecutor::new(
            root.path(),
            [FakeTurn::ResumeFailure, FakeTurn::Success("new-session")],
        );
        let mut supervisor = supervisor(&config, &executor);

        assert_eq!(run_single_poll(&mut supervisor), Duration::ZERO);
        let executions = executor.executions.lock().expect("executions");
        assert_eq!(executions.len(), 2);
        assert!(executions[0].argv.contains(&"--resume".to_string()));
        assert!(!executions[1].argv.contains(&"--resume".to_string()));
        assert_eq!(
            fs::read_to_string(config.coordinator_dir().join(SESSION_FILE)).expect("session"),
            "new-session"
        );
    }

    #[test]
    fn stop_file_ends_the_loop_and_writes_stopped_status() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        let lock = CoordinatorLock::acquire(&config).expect("lock");
        let executor = FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Stop]);
        executor.stop_on_child(config.coordinator_dir().join(STOP_FILE));
        add_message(&config);
        let launcher = RoleLauncher::new(skills_dir(), &executor);
        run_locked(&config, launcher, lock).expect("run until stop");

        assert_eq!(
            CoordinatorStatus::read(&config).expect("status").state,
            CoordinatorState::Stopped
        );
    }

    #[test]
    fn a_second_supervisor_for_a_lab_is_refused() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        let first = CoordinatorLock::acquire(&config).expect("first lock");
        assert!(matches!(
            CoordinatorLock::acquire(&config),
            Err(CoordinatorError::AlreadyRunning)
        ));
        drop(first);
        assert!(CoordinatorLock::acquire(&config).is_ok());
    }

    #[test]
    fn successful_turn_status_is_running_during_launch() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        add_message(&config);
        let executor = FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Success("session")]);
        executor.observe_status(config.coordinator_dir().join(STATUS_FILE));
        let mut supervisor = supervisor(&config, &executor);

        run_single_poll(&mut supervisor);

        assert!(*executor.running_status_seen.lock().expect("running status"));
        assert_eq!(
            CoordinatorStatus::read(&config).expect("status").state,
            CoordinatorState::Idle
        );
    }

    #[test]
    fn timed_out_turn_is_not_acked() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        add_message(&config);
        let executor = FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Timeout]);
        let mut supervisor = supervisor(&config, &executor);

        assert_eq!(run_single_poll(&mut supervisor), Duration::from_secs(5));
        assert_eq!(
            Router::open(config.state_dir.join("routing"))
                .expect("router")
                .inbox(&HubRole::LabCoordinator)
                .expect("inbox")
                .len(),
            1
        );
        assert_eq!(
            CoordinatorStatus::read(&config)
                .expect("status")
                .last_error
                .as_deref(),
            Some("coordinator turn timed out")
        );
    }

    #[test]
    fn paused_scheduler_does_not_launch_or_ack_a_turn() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = make_config(root.path());
        add_message(&config);
        let scheduler = Scheduler::open(
            config
                .state_dir
                .join("labs")
                .join(&config.lab)
                .join("scheduler"),
        )
        .expect("scheduler");
        scheduler.pause().expect("pause");
        let executor = FakeRoleLaunchExecutor::new(root.path(), [FakeTurn::Success("session")]);
        let mut supervisor = supervisor(&config, &executor);

        assert_eq!(run_single_poll(&mut supervisor), Duration::from_secs(5));

        assert!(executor.executions.lock().expect("executions").is_empty());
        assert_eq!(
            Router::open(config.state_dir.join("routing"))
                .expect("router")
                .inbox(&HubRole::LabCoordinator)
                .expect("inbox")
                .len(),
            1
        );
    }
}
