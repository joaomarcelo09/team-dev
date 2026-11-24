//! Launches factory roles inside a scoped lab-world exec.

use std::{
    collections::BTreeMap,
    fmt, fs,
    path::{Path, PathBuf},
    time::Duration,
};

use jg_sandbox::{ExecScope, ScopeAccess};
use jgf_core::{FileSecretStore, Project, ProjectId, Secret, SecretStore, env_for_project};
use jgf_hub::{LaunchToken, Role as HubRole, WorkerKind, broker_exec::GITHUB_TOKEN_KEY};

use crate::{e2e::KillSwitch, sandbox::ClaudeSandbox};

pub use crate::sandbox::ChildRun;

/// A factory role and the skill directory that defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Owns the lab-wide queue and user-facing reports.
    LabCoordinator,
    /// Coordinates one project's workers.
    CoCoordinator,
    /// Implements one assigned project piece.
    Coder,
    /// Reviews one assigned project piece.
    Reviewer,
    /// Tests the project's acceptance criteria.
    Tester,
    /// Performs authorized host-side Git broker actions.
    GitCoder,
    /// Checks lab health for the host watchdog.
    WakeUp,
}

impl Role {
    /// Directory name containing this role's `SKILL.md`.
    pub fn skill_directory(self) -> &'static str {
        match self {
            Self::LabCoordinator => "lab-coordinator",
            Self::CoCoordinator => "co-coordinator",
            Self::Coder => "coder",
            Self::Reviewer => "reviewer",
            Self::Tester => "tester",
            Self::GitCoder => "git-coder",
            Self::WakeUp => "wake-up",
        }
    }

    fn worktree_access(self) -> ScopeAccess {
        match self {
            Self::Coder | Self::GitCoder => ScopeAccess::ReadWrite,
            Self::LabCoordinator
            | Self::CoCoordinator
            | Self::Reviewer
            | Self::Tester
            | Self::WakeUp => ScopeAccess::ReadOnly,
        }
    }
}

/// One role launch requested by the lab runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleLaunch {
    /// Role to start.
    pub role: Role,
    /// Project whose worktree is available to the role.
    pub project_id: String,
    /// Worker identity for audit attribution and its private scratch home.
    pub worker_id: String,
    /// Name of the project's worktree.
    pub worktree_name: String,
    /// Task prompt passed to Claude.
    pub prompt: String,
    /// Wall-clock limit for this role process.
    pub timeout: Duration,
    /// Maximum agentic turns Claude may take.
    pub max_turns: u32,
    /// Maximum Claude budget in US dollars.
    pub max_budget_usd: f64,
}

/// One prepared role invocation, passed to the sandbox executor.
#[derive(Debug, Clone, PartialEq)]
pub struct RoleExecution {
    /// Role started by this invocation.
    pub role: Role,
    /// Project whose worktree is mounted.
    pub project_id: String,
    /// Worker identity used for audit attribution.
    pub worker_id: String,
    /// Host worktree path, validated by the sandbox runner.
    pub worktree: PathBuf,
    /// Private scratch home for this role.
    pub scratch_home: PathBuf,
    /// Claude executable arguments, including the skill and standards prompt.
    pub argv: Vec<String>,
    /// Process wall-clock limit.
    pub timeout: Duration,
    /// Only this project worktree is mounted into the scoped exec.
    pub scope: ExecScope,
    /// Per-launch hub socket credentials, redacted from debug output.
    pub(crate) message_surface: Option<MessageSurface>,
    /// Per-launch broker socket credentials, redacted from debug output.
    pub(crate) broker_surface: Option<BrokerSurface>,
    /// Project environment values, redacted from debug output.
    pub(crate) project_environment: ProjectEnvironment,
}

#[derive(Clone, Default)]
pub(crate) struct ProjectEnvironment(BTreeMap<String, Secret>);

impl PartialEq for ProjectEnvironment {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().all(|(key, value)| {
                other
                    .0
                    .get(key)
                    .is_some_and(|other_value| value.expose() == other_value.expose())
            })
    }
}

impl Eq for ProjectEnvironment {}

impl ProjectEnvironment {
    fn from_values(values: BTreeMap<String, Secret>) -> Self {
        Self(values)
    }

    pub(crate) fn values(&self) -> &BTreeMap<String, Secret> {
        &self.0
    }
}

impl fmt::Debug for ProjectEnvironment {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectEnvironment")
            .field("values", &"[redacted]")
            .finish()
    }
}

/// Per-launch credentials passed only through the child's process environment.
#[derive(Clone, PartialEq, Eq)]
pub struct MessageSurface {
    socket_path: PathBuf,
    token: LaunchToken,
}

impl MessageSurface {
    /// Builds a message surface from a host-issued token and socket path.
    pub fn new(socket_path: impl Into<PathBuf>, token: LaunchToken) -> Self {
        Self {
            socket_path: socket_path.into(),
            token,
        }
    }

    pub(crate) fn token(&self) -> &LaunchToken {
        &self.token
    }

    fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "JG_HUB_SOCKET".to_string(),
                self.socket_path.to_string_lossy().into_owned(),
            ),
            ("JG_HUB_TOKEN".to_string(), self.token.expose().to_string()),
        ])
    }
}

impl fmt::Debug for MessageSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MessageSurface")
            .field("socket_path", &self.socket_path)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Per-launch credentials passed only through the child's process environment.
#[derive(Clone, PartialEq, Eq)]
pub struct BrokerSurface {
    socket_path: PathBuf,
    token: LaunchToken,
}

impl BrokerSurface {
    /// Builds a broker surface from a host-issued token and socket path.
    pub fn new(socket_path: impl Into<PathBuf>, token: LaunchToken) -> Self {
        Self {
            socket_path: socket_path.into(),
            token,
        }
    }

    pub(crate) fn token(&self) -> &LaunchToken {
        &self.token
    }

    fn environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "JG_BROKER_SOCKET".to_string(),
                self.socket_path.to_string_lossy().into_owned(),
            ),
            (
                "JG_BROKER_TOKEN".to_string(),
                self.token.expose().to_string(),
            ),
        ])
    }
}

impl fmt::Debug for BrokerSurface {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrokerSurface")
            .field("socket_path", &self.socket_path)
            .field("token", &"[redacted]")
            .finish()
    }
}

/// Sandbox operations needed by the role launcher.
pub trait RoleLaunchExecutor {
    /// Returns the active lab directory or fails when the world is unavailable.
    fn lab_dir(&self) -> Result<PathBuf, String>;
    /// Writes the single tagged role-start audit event.
    fn record_role_start(&self, project: &str, worker: &str, role: Role) -> Result<(), String>;
    /// Starts the role in the requested scoped exec.
    fn run_role(&self, execution: RoleExecution, kill: Option<&dyn KillSwitch>) -> ChildRun;
    /// Returns allowed project environment values for the selected project.
    fn project_environment(&self, _project_id: &str) -> Result<BTreeMap<String, Secret>, String> {
        Ok(BTreeMap::new())
    }

    /// Registers a host-selected message identity for the duration of a launch.
    fn register_message_role(&self, _role: HubRole) -> Result<Option<MessageSurface>, String> {
        Ok(None)
    }

    /// Revokes the identity returned by [`RoleLaunchExecutor::register_message_role`].
    fn revoke_message_role(&self, _surface: &MessageSurface) {}

    /// Registers a broker capability for the selected project and worker.
    fn register_broker_role(
        &self,
        _project: &str,
        _worker: &str,
    ) -> Result<Option<BrokerSurface>, String> {
        Ok(None)
    }

    /// Revokes the capability returned by [`RoleLaunchExecutor::register_broker_role`].
    fn revoke_broker_role(&self, _surface: &BrokerSurface) {}
}

/// Loads role instructions and starts the role through a sandbox executor.
pub struct RoleLauncher<'a> {
    skills_dir: PathBuf,
    executor: &'a dyn RoleLaunchExecutor,
}

impl<'a> RoleLauncher<'a> {
    /// Creates a launcher for skill files beneath `skills_dir`.
    pub fn new(skills_dir: impl Into<PathBuf>, executor: &'a dyn RoleLaunchExecutor) -> Self {
        Self {
            skills_dir: skills_dir.into(),
            executor,
        }
    }

    /// Loads the skill and standards, records the start, and runs the role.
    pub fn launch(&self, request: &RoleLaunch, kill: Option<&dyn KillSwitch>) -> ChildRun {
        let execution = match self.prepare(request) {
            Ok(execution) => execution,
            Err(error) => return failed_launch(error),
        };
        if let Err(error) = self.executor.record_role_start(
            &execution.project_id,
            &execution.worker_id,
            execution.role,
        ) {
            return failed_launch(error);
        }
        let mut execution = execution;
        let broker_surface = if execution.role == Role::GitCoder {
            match self
                .executor
                .register_broker_role(&execution.project_id, &execution.worker_id)
            {
                Ok(surface) => surface,
                Err(error) => return failed_launch(error),
            }
        } else {
            None
        };
        execution.broker_surface = broker_surface.clone();
        let message_role = match execution.message_role() {
            Ok(role) => role,
            Err(error) => {
                if let Some(surface) = broker_surface.as_ref() {
                    self.executor.revoke_broker_role(surface);
                }
                return failed_launch(error);
            }
        };
        let message_surface = match message_role {
            Some(role) => match self.executor.register_message_role(role) {
                Ok(surface) => surface,
                Err(error) => {
                    if let Some(surface) = broker_surface.as_ref() {
                        self.executor.revoke_broker_role(surface);
                    }
                    return failed_launch(error);
                }
            },
            None => None,
        };
        execution.message_surface = message_surface.clone();
        let result = self.executor.run_role(execution, kill);
        if let Some(surface) = broker_surface.as_ref() {
            self.executor.revoke_broker_role(surface);
        }
        if let Some(surface) = message_surface.as_ref() {
            self.executor.revoke_message_role(surface);
        }
        result
    }

    fn prepare(&self, request: &RoleLaunch) -> Result<RoleExecution, String> {
        validate_identifier("project id", &request.project_id)?;
        validate_identifier("worker id", &request.worker_id)?;
        validate_identifier("worktree name", &request.worktree_name)?;

        let lab_dir = fs::canonicalize(self.executor.lab_dir()?)
            .map_err(|error| format!("could not resolve role lab directory: {error}"))?;
        let project_environment = self
            .executor
            .project_environment(&request.project_id)
            .map_err(|_| "could not load project environment".to_string())?
            .into_iter()
            .filter(|(key, _)| !is_reserved_role_environment_key(key))
            .collect();
        let skill = fs::read_to_string(
            self.skills_dir
                .join(request.role.skill_directory())
                .join("SKILL.md"),
        )
        .map_err(|error| {
            format!(
                "could not load {} role skill: {error}",
                request.role.skill_directory()
            )
        })?;
        let standards = fs::read_to_string(self.skills_dir.join("standards/STANDARDS.md"))
            .map_err(|error| format!("could not load lab standards: {error}"))?;

        let worktree = lab_dir
            .join("projects")
            .join(&request.project_id)
            .join("worktrees")
            .join(&request.worktree_name);
        let scratch_home = lab_dir
            .join("scratch")
            .join(&request.project_id)
            .join(&request.worker_id);
        if worktree.starts_with(&scratch_home) || scratch_home.starts_with(&worktree) {
            return Err("role worktree and scratch home must be separate".to_string());
        }

        let relative_worktree = worktree
            .strip_prefix(&lab_dir)
            .map_err(|_| "role worktree is outside the lab".to_string())?;
        let guest_worktree = Path::new("/lab").join(relative_worktree);
        let relative_scratch = scratch_home
            .strip_prefix(&lab_dir)
            .map_err(|_| "role scratch home is outside the lab".to_string())?;
        let guest_scratch = Path::new("/lab").join(relative_scratch);
        let scope = build_role_scope(
            request.role,
            &request.project_id,
            &lab_dir,
            &worktree,
            &guest_worktree,
        )?
        .with_scratch_home(guest_scratch);

        let system_prompt = format!("{skill}\n\n---\n\n# Lab standards\n\n{standards}");
        let argv = vec![
            "claude".to_string(),
            "-p".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "--no-session-persistence".to_string(),
            "--max-turns".to_string(),
            request.max_turns.to_string(),
            "--max-budget-usd".to_string(),
            format!("{:.2}", request.max_budget_usd),
            "--append-system-prompt".to_string(),
            system_prompt,
            request.prompt.clone(),
        ];

        Ok(RoleExecution {
            role: request.role,
            project_id: request.project_id.clone(),
            worker_id: request.worker_id.clone(),
            worktree,
            scratch_home,
            argv,
            timeout: request.timeout,
            scope,
            message_surface: None,
            broker_surface: None,
            project_environment: ProjectEnvironment::from_values(project_environment),
        })
    }
}

impl RoleExecution {
    fn message_role(&self) -> Result<Option<HubRole>, String> {
        let project = ProjectId::parse(&self.project_id).map_err(|error| error.to_string())?;
        match self.role {
            Role::LabCoordinator => Ok(Some(HubRole::LabCoordinator)),
            Role::CoCoordinator => Ok(Some(HubRole::CoCoordinator(project))),
            Role::Coder | Role::Reviewer | Role::Tester | Role::GitCoder => {
                let kind = match self.role {
                    Role::Coder => WorkerKind::Coder,
                    Role::Reviewer => WorkerKind::Reviewer,
                    Role::Tester => WorkerKind::Tester,
                    Role::GitCoder => WorkerKind::GitCoder,
                    _ => unreachable!("worker role matched above"),
                };
                HubRole::worker(project, self.worker_id.clone(), kind)
                    .map(Some)
                    .map_err(|error| error.to_string())
            }
            Role::WakeUp => Ok(None),
        }
    }

    pub(crate) fn message_environment(&self) -> BTreeMap<String, String> {
        self.message_surface
            .as_ref()
            .map(MessageSurface::environment)
            .unwrap_or_default()
    }

    pub(crate) fn broker_environment(&self) -> BTreeMap<String, String> {
        self.broker_surface
            .as_ref()
            .map(BrokerSurface::environment)
            .unwrap_or_default()
    }
}

fn project_environment(
    store: &dyn SecretStore,
    project: &Project,
) -> Result<BTreeMap<String, Secret>, String> {
    let values = env_for_project(store, project)
        .map_err(|_| "could not load project environment".to_string())?;
    Ok(values
        .into_iter()
        .filter(|(key, _)| !is_reserved_role_environment_key(key.as_str()))
        .map(|(key, secret)| (key.as_str().to_string(), secret))
        .collect())
}

fn is_reserved_role_environment_key(key: &str) -> bool {
    matches!(
        key,
        GITHUB_TOKEN_KEY
            | "HTTP_PROXY"
            | "HTTPS_PROXY"
            | "ALL_PROXY"
            | "NO_PROXY"
            | "http_proxy"
            | "https_proxy"
            | "all_proxy"
            | "no_proxy"
    )
}

fn validate_identifier(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || value == "."
        || value == ".."
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(format!(
            "{name} must be 1 to 64 ASCII letters, digits, '-' or '_'"
        ));
    }
    Ok(())
}

fn build_role_scope(
    role: Role,
    project: &str,
    lab_dir: &Path,
    worktree: &Path,
    guest_worktree: &Path,
) -> Result<ExecScope, String> {
    let mut scope =
        ExecScope::new(role.worktree_access(), guest_worktree).with_project(project.to_string());
    let git_entry = worktree.join(".git");
    let metadata = match fs::symlink_metadata(&git_entry) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(scope),
        Err(error) => {
            return Err(format!(
                "could not inspect role worktree git metadata: {error}"
            ));
        }
    };

    if metadata.is_dir() {
        if scope.access == ScopeAccess::ReadWrite {
            return Err(
                "writable role worktrees must use linked git metadata so .git remains read-only"
                    .to_string(),
            );
        }
        return Ok(scope);
    }
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("role worktree .git must be a plain file or directory".to_string());
    }

    let pointer = fs::read_to_string(&git_entry)
        .map_err(|error| format!("could not read role worktree git pointer: {error}"))?;
    let git_dir = pointer
        .trim()
        .strip_prefix("gitdir: ")
        .ok_or_else(|| "role worktree .git file has an invalid gitdir pointer".to_string())?;
    let git_dir = Path::new(git_dir);
    let git_dir = if git_dir.is_absolute() {
        git_dir.to_path_buf()
    } else {
        worktree.join(git_dir)
    };
    let git_dir = fs::canonicalize(&git_dir)
        .map_err(|error| format!("could not resolve role worktree git directory: {error}"))?;
    if !git_dir.is_dir() || !git_dir.starts_with(lab_dir) {
        return Err("role worktree git directory is outside the lab".to_string());
    }

    let common_dir_file = git_dir.join("commondir");
    let common_dir = match fs::read_to_string(&common_dir_file) {
        Ok(path) => {
            let path = Path::new(path.trim());
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                git_dir.join(path)
            };
            fs::canonicalize(&path).map_err(|error| {
                format!("could not resolve role worktree common git directory: {error}")
            })?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir,
        Err(error) => {
            return Err(format!(
                "could not read role worktree common git directory: {error}"
            ));
        }
    };
    if !common_dir.is_dir() {
        return Err("role worktree common git directory is not a directory".to_string());
    }
    let relative_common_dir = common_dir
        .strip_prefix(lab_dir)
        .map_err(|_| "role worktree common git directory is outside the lab".to_string())?;
    // Common git metadata is always mounted read-only; pushes and other outward actions use the broker.
    scope = scope.with_common_dir(Path::new("/lab").join(relative_common_dir));
    Ok(scope)
}

fn failed_launch(error: String) -> ChildRun {
    ChildRun {
        spawn_error: Some(error),
        ..ChildRun::default()
    }
}

impl RoleLaunchExecutor for ClaudeSandbox {
    fn lab_dir(&self) -> Result<PathBuf, String> {
        self.role_lab_dir()
    }

    fn record_role_start(&self, project: &str, worker: &str, role: Role) -> Result<(), String> {
        self.audit_role_start(project, worker, role.skill_directory())
    }

    fn run_role(&self, execution: RoleExecution, kill: Option<&dyn KillSwitch>) -> ChildRun {
        self.execute_role(execution, kill)
    }

    fn project_environment(&self, project: &str) -> Result<BTreeMap<String, Secret>, String> {
        let project_id = ProjectId::parse(project)
            .map_err(|_| "could not load project configuration".to_string())?;
        let root = self.role_project_root();
        let project = jgf_core::registry::load_project(root, &project_id)
            .map_err(|_| "could not load project configuration".to_string())?;
        project_environment(&FileSecretStore::new(root), &project)
    }

    fn register_message_role(&self, role: HubRole) -> Result<Option<MessageSurface>, String> {
        self.register_hub_role(role).map(Some)
    }

    fn revoke_message_role(&self, surface: &MessageSurface) {
        self.revoke_hub_role(surface);
    }

    fn register_broker_role(
        &self,
        project: &str,
        worker: &str,
    ) -> Result<Option<BrokerSurface>, String> {
        self.register_github_broker_role(project, worker).map(Some)
    }

    fn revoke_broker_role(&self, surface: &BrokerSurface) {
        self.revoke_github_broker_role(surface);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use jgf_core::{EnvProfile, ProjectStage, Secret, SecretKey, SecretScope};
    use jgf_hub::{HubClient, HubClientError, HubShim, Router};

    use super::*;

    #[derive(Default)]
    struct RecordingExecutor {
        lab_dir: PathBuf,
        launches: Mutex<Vec<RoleExecution>>,
        starts: Mutex<Vec<(String, String, Role)>>,
        project: Option<Project>,
        secret_store: Option<FileSecretStore>,
        extra_environment: BTreeMap<String, Secret>,
        environment_error: Option<String>,
    }

    impl RoleLaunchExecutor for RecordingExecutor {
        fn lab_dir(&self) -> Result<PathBuf, String> {
            Ok(self.lab_dir.clone())
        }

        fn record_role_start(&self, project: &str, worker: &str, role: Role) -> Result<(), String> {
            self.starts.lock().expect("start records").push((
                project.to_string(),
                worker.to_string(),
                role,
            ));
            Ok(())
        }

        fn run_role(&self, execution: RoleExecution, _kill: Option<&dyn KillSwitch>) -> ChildRun {
            self.launches
                .lock()
                .expect("launch records")
                .push(execution);
            ChildRun::default()
        }

        fn project_environment(
            &self,
            _project_id: &str,
        ) -> Result<BTreeMap<String, Secret>, String> {
            match &self.environment_error {
                Some(error) => Err(error.clone()),
                None => {
                    let mut env = match (&self.project, &self.secret_store) {
                        (Some(project), Some(store)) => project_environment(store, project),
                        _ => Ok(BTreeMap::new()),
                    }?;
                    env.extend(self.extra_environment.clone());
                    Ok(env)
                }
            }
        }
    }

    struct HubRecordingExecutor {
        lab_dir: PathBuf,
        hub: HubShim,
        launches: Mutex<Vec<RoleExecution>>,
        issued_token: Mutex<Option<LaunchToken>>,
        issued_broker_token: Mutex<Option<LaunchToken>>,
    }

    impl RoleLaunchExecutor for HubRecordingExecutor {
        fn lab_dir(&self) -> Result<PathBuf, String> {
            Ok(self.lab_dir.clone())
        }

        fn record_role_start(
            &self,
            _project: &str,
            _worker: &str,
            _role: Role,
        ) -> Result<(), String> {
            Ok(())
        }

        fn run_role(&self, execution: RoleExecution, _kill: Option<&dyn KillSwitch>) -> ChildRun {
            *self.issued_token.lock().expect("issued token") = execution
                .message_surface
                .as_ref()
                .map(|surface| surface.token().clone());
            self.launches.lock().expect("launches").push(execution);
            ChildRun::default()
        }

        fn register_message_role(&self, role: HubRole) -> Result<Option<MessageSurface>, String> {
            let token = self
                .hub
                .register_role(role)
                .map_err(|error| error.to_string())?;
            Ok(Some(MessageSurface::new(
                self.hub.socket_path().to_path_buf(),
                token,
            )))
        }

        fn revoke_message_role(&self, surface: &MessageSurface) {
            self.hub.revoke(surface.token());
        }

        fn register_broker_role(
            &self,
            project: &str,
            worker: &str,
        ) -> Result<Option<BrokerSurface>, String> {
            let project = jgf_core::ProjectId::parse(project).map_err(|error| error.to_string())?;
            let role = HubRole::worker(project, worker, WorkerKind::GitCoder)
                .map_err(|error| error.to_string())?;
            let token = self
                .hub
                .register_role(role)
                .map_err(|error| error.to_string())?;
            *self.issued_broker_token.lock().expect("broker token") = Some(token.clone());
            Ok(Some(BrokerSurface::new(
                self.hub.socket_path().to_path_buf(),
                token,
            )))
        }

        fn revoke_broker_role(&self, surface: &BrokerSurface) {
            self.hub.revoke(surface.token());
        }
    }

    fn request(role: Role) -> RoleLaunch {
        RoleLaunch {
            role,
            project_id: "project-a".to_string(),
            worker_id: "worker-1".to_string(),
            worktree_name: "task-1".to_string(),
            prompt: "Do the assigned work".to_string(),
            timeout: Duration::from_secs(120),
            max_turns: 7,
            max_budget_usd: 1.25,
        }
    }

    fn sample_project(allowed: &[&str]) -> Project {
        Project {
            id: ProjectId::parse("project-a").expect("valid id"),
            name: "Example".to_string(),
            spec_path: PathBuf::from("/tmp/spec.md"),
            spec_sha256: None,
            repos: Vec::new(),
            stage: ProjectStage::Mvp,
            env_profile: EnvProfile {
                allowed_keys: allowed.iter().map(|key| (*key).to_string()).collect(),
            },
            recipes: Vec::new(),
        }
    }

    #[test]
    fn launched_role_gets_only_allowed_project_environment_and_redacts_values() {
        let temp = tempfile::tempdir().expect("secret store");
        let store = FileSecretStore::new(temp.path());
        let project = sample_project(&["ALLOWED_KEY", GITHUB_TOKEN_KEY, "HTTPS_PROXY"]);
        let scope = SecretScope::Project(project.id.clone());
        for (key, value) in [
            ("ALLOWED_KEY", "allowed-value"),
            ("DISALLOWED_KEY", "disallowed-value"),
            (GITHUB_TOKEN_KEY, "github-token-value"),
            ("HTTPS_PROXY", "untrusted-proxy-value"),
        ] {
            store
                .set(
                    &scope,
                    &SecretKey::parse(key).expect("valid key"),
                    Secret::new(value),
                )
                .expect("store secret");
        }
        let env = project_environment(&store, &project).expect("project environment");
        assert!(
            env.get("ALLOWED_KEY")
                .is_some_and(|value| value.expose() == "allowed-value")
        );
        assert!(!env.contains_key("DISALLOWED_KEY"));
        assert!(!env.contains_key(GITHUB_TOKEN_KEY));
        assert!(!env.contains_key("HTTPS_PROXY"));

        let lab_dir = temp.path().join("lab");
        fs::create_dir_all(lab_dir.join("projects/project-a/worktrees/task-1")).expect("worktree");
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let extra_environment = BTreeMap::from([
            (
                GITHUB_TOKEN_KEY.to_string(),
                Secret::new("github-token-value"),
            ),
            (
                "HTTPS_PROXY".to_string(),
                Secret::new("untrusted-proxy-value"),
            ),
        ]);
        let executor = RecordingExecutor {
            lab_dir,
            project: Some(project),
            secret_store: Some(store),
            extra_environment,
            ..RecordingExecutor::default()
        };
        let launcher = RoleLauncher::new(skills_dir, &executor);

        let result = launcher.launch(&request(Role::Coder), None);

        assert!(result.spawn_error.is_none());
        let execution = executor.launches.lock().expect("launches").pop().unwrap();
        assert!(
            execution
                .project_environment
                .values()
                .get("ALLOWED_KEY")
                .is_some_and(|value| value.expose() == "allowed-value")
        );
        assert!(
            !execution
                .project_environment
                .values()
                .contains_key("DISALLOWED_KEY")
        );
        assert!(
            !execution
                .project_environment
                .values()
                .contains_key(GITHUB_TOKEN_KEY)
        );
        assert!(
            !execution
                .project_environment
                .values()
                .contains_key("HTTPS_PROXY")
        );
        let debug = format!("{execution:?}");
        let allowed_value = "allowed-value";
        let github_token_value = "github-token-value";
        let proxy_value = "untrusted-proxy-value";
        assert!(!debug.contains(allowed_value));
        assert!(!debug.contains(github_token_value));
        assert!(!debug.contains(proxy_value));
        assert!(
            !format!(
                "{:?}",
                executor.starts.lock().expect("audit-like start records")
            )
            .contains(allowed_value)
        );
    }

    #[test]
    fn project_environment_errors_do_not_leak_values() {
        let temp = tempfile::tempdir().expect("lab directory");
        let lab_dir = temp.path().join("lab");
        fs::create_dir_all(lab_dir.join("projects/project-a/worktrees/task-1")).expect("worktree");
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let executor = RecordingExecutor {
            lab_dir,
            environment_error: Some("secret-error-value".to_string()),
            ..RecordingExecutor::default()
        };
        let launcher = RoleLauncher::new(skills_dir, &executor);

        let result = launcher.launch(&request(Role::Coder), None);

        let error = result.spawn_error.expect("environment error");
        assert!(!error.contains("secret-error-value"));
        assert_eq!(error, "could not load project environment");
        assert!(executor.launches.lock().expect("launches").is_empty());
    }

    #[test]
    fn sandbox_loads_the_project_profile_before_preparing_role_environment() {
        let temp = tempfile::tempdir().expect("project store");
        let project = sample_project(&["ALLOWED_KEY", GITHUB_TOKEN_KEY, "HTTPS_PROXY"]);
        jgf_core::registry::save_project(temp.path(), &project).expect("save project");
        let store = FileSecretStore::new(temp.path());
        let scope = SecretScope::Project(project.id.clone());
        for (key, value) in [
            ("ALLOWED_KEY", "allowed-value"),
            ("DISALLOWED_KEY", "disallowed-value"),
            (GITHUB_TOKEN_KEY, "github-token-value"),
            ("HTTPS_PROXY", "untrusted-proxy-value"),
        ] {
            store
                .set(
                    &scope,
                    &SecretKey::parse(key).expect("valid key"),
                    Secret::new(value),
                )
                .expect("store secret");
        }
        let sandbox = ClaudeSandbox::new(temp.path(), "claude");

        let env = RoleLaunchExecutor::project_environment(&sandbox, "project-a")
            .expect("project environment");

        assert!(
            env.get("ALLOWED_KEY")
                .is_some_and(|value| value.expose() == "allowed-value")
        );
        assert!(!env.contains_key("DISALLOWED_KEY"));
        assert!(!env.contains_key(GITHUB_TOKEN_KEY));
        assert!(!env.contains_key("HTTPS_PROXY"));
        let debug = format!("{env:?}");
        assert!(!debug.contains("allowed-value"));
        assert!(!debug.contains("github-token-value"));
        assert!(!debug.contains("untrusted-proxy-value"));
    }

    #[test]
    fn every_role_loads_its_skill_and_launches_with_full_allow_and_expected_scope() {
        let lab = tempfile::tempdir().expect("lab directory");
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let worktree = lab.path().join("projects/project-a/worktrees/task-1");
        let git_dir = lab
            .path()
            .join("projects/project-a/repository/.git/worktrees/task-1");
        fs::create_dir_all(&worktree).expect("worktree");
        fs::create_dir_all(&git_dir).expect("worktree git directory");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .expect("git pointer");
        fs::write(git_dir.join("commondir"), "../..\n").expect("common git directory");
        let executor = RecordingExecutor {
            lab_dir: lab.path().to_path_buf(),
            ..RecordingExecutor::default()
        };
        let launcher = RoleLauncher::new(&skills_dir, &executor);
        let roles = [
            Role::LabCoordinator,
            Role::CoCoordinator,
            Role::Coder,
            Role::Reviewer,
            Role::Tester,
            Role::GitCoder,
            Role::WakeUp,
        ];

        for role in roles {
            let result = launcher.launch(&request(role), None);
            assert!(result.spawn_error.is_none());
        }

        let launches = executor.launches.lock().expect("launches");
        assert_eq!(launches.len(), roles.len());
        for (execution, role) in launches.iter().zip(roles) {
            let skill =
                fs::read_to_string(skills_dir.join(role.skill_directory()).join("SKILL.md"))
                    .expect("role skill");
            let standards =
                fs::read_to_string(skills_dir.join("standards/STANDARDS.md")).expect("standards");
            let system_prompt = execution.argv.get(9).expect("appended system prompt");
            assert!(system_prompt.contains(&skill));
            assert!(system_prompt.contains(&standards));
            assert!(
                execution
                    .argv
                    .contains(&"--dangerously-skip-permissions".to_string())
            );
            assert_eq!(
                execution.argv.get(4).map(String::as_str),
                Some("--max-turns")
            );
            assert_eq!(execution.argv.get(5).map(String::as_str), Some("7"));
            assert_eq!(execution.argv.get(7).map(String::as_str), Some("1.25"));
            assert_eq!(
                execution.argv.get(8).map(String::as_str),
                Some("--append-system-prompt")
            );
            assert_eq!(
                execution.argv.last().map(String::as_str),
                Some("Do the assigned work")
            );
            assert_eq!(
                execution.scope.worktree.as_path(),
                "/lab/projects/project-a/worktrees/task-1"
            );
            assert_eq!(
                execution.scope.common_dir.as_deref(),
                Some(Path::new("/lab/projects/project-a/repository/.git"))
            );
            assert_eq!(
                execution.scope.scratch_home.as_deref(),
                Some(Path::new("/lab/scratch/project-a/worker-1"))
            );
            assert_eq!(execution.scope.project.as_deref(), Some("project-a"));
            assert_eq!(
                execution.scope.access,
                match role {
                    Role::Coder | Role::GitCoder => ScopeAccess::ReadWrite,
                    _ => ScopeAccess::ReadOnly,
                }
            );
            assert_eq!(
                execution.scratch_home,
                lab.path().join("scratch/project-a/worker-1")
            );
        }
        assert_eq!(executor.starts.lock().expect("starts").len(), roles.len());
    }

    #[test]
    fn launcher_sets_private_message_environment_and_revokes_token_after_exit() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let worktree = lab_dir.join("projects/project-a/worktrees/task-1");
        fs::create_dir_all(&worktree).expect("worktree");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let hub = HubShim::start(temp.path().join("hub.sock"), router, |_| {}).expect("hub");
        let executor = HubRecordingExecutor {
            lab_dir: lab_dir.clone(),
            hub,
            launches: Mutex::new(Vec::new()),
            issued_token: Mutex::new(None),
            issued_broker_token: Mutex::new(None),
        };
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let launcher = RoleLauncher::new(skills_dir, &executor);

        let result = launcher.launch(&request(Role::Coder), None);
        assert!(result.spawn_error.is_none());
        let execution = executor.launches.lock().expect("launches").pop().unwrap();
        let environment = execution.message_environment();
        assert_eq!(
            environment.get("JG_HUB_SOCKET").map(String::as_str),
            Some(executor.hub.socket_path().to_str().expect("socket path"))
        );
        let token = executor
            .issued_token
            .lock()
            .expect("issued token")
            .clone()
            .expect("registered token");
        assert_eq!(
            environment.get("JG_HUB_TOKEN").map(String::as_str),
            Some(token.expose())
        );
        assert!(!format!("{execution:?}").contains(token.expose()));

        let client = HubClient::new(executor.hub.socket_path(), token);
        assert!(matches!(
            client.inbox(),
            Err(HubClientError::Shim(jgf_hub::ShimError::UnknownToken))
        ));
    }

    #[test]
    fn git_coder_receives_private_broker_environment_and_revoked_token() {
        let temp = tempfile::tempdir().expect("temp dir");
        let lab_dir = temp.path().join("lab");
        let worktree = lab_dir.join("projects/project-a/worktrees/task-1");
        fs::create_dir_all(&worktree).expect("worktree");
        let router = Router::open(temp.path().join("mail")).expect("router");
        let hub = HubShim::start(temp.path().join("hub.sock"), router, |_| {}).expect("hub");
        let executor = HubRecordingExecutor {
            lab_dir: lab_dir.clone(),
            hub,
            launches: Mutex::new(Vec::new()),
            issued_token: Mutex::new(None),
            issued_broker_token: Mutex::new(None),
        };
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let launcher = RoleLauncher::new(skills_dir, &executor);

        let result = launcher.launch(&request(Role::GitCoder), None);
        assert!(result.spawn_error.is_none());
        let execution = executor.launches.lock().expect("launches").pop().unwrap();
        let environment = execution.broker_environment();
        assert_eq!(
            environment.get("JG_BROKER_SOCKET").map(String::as_str),
            Some(executor.hub.socket_path().to_str().expect("socket path"))
        );
        let token = executor
            .issued_broker_token
            .lock()
            .expect("issued broker token")
            .clone()
            .expect("registered broker token");
        assert_eq!(
            environment.get("JG_BROKER_TOKEN").map(String::as_str),
            Some(token.expose())
        );
        assert!(!format!("{execution:?}").contains(token.expose()));

        let client = HubClient::new(executor.hub.socket_path(), token);
        assert!(matches!(
            client.inbox(),
            Err(HubClientError::Shim(jgf_hub::ShimError::UnknownToken))
        ));
    }

    #[test]
    fn unsafe_path_identifiers_are_rejected_before_starting_a_role() {
        let lab = tempfile::tempdir().expect("lab directory");
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let executor = RecordingExecutor {
            lab_dir: lab.path().to_path_buf(),
            ..RecordingExecutor::default()
        };
        let launcher = RoleLauncher::new(skills_dir, &executor);
        let mut launch = request(Role::Coder);
        launch.worktree_name = "../outside".to_string();

        let result = launcher.launch(&launch, None);

        assert!(result.spawn_error.is_some());
        assert!(executor.launches.lock().expect("launches").is_empty());
        assert!(executor.starts.lock().expect("starts").is_empty());
    }

    #[test]
    fn missing_world_fails_closed_without_starting_claude() {
        let temp = tempfile::tempdir().expect("state directory");
        let sandbox = ClaudeSandbox::new(temp.path().join("state"), "claude");
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let launcher = RoleLauncher::new(skills_dir, &sandbox);

        let result = launcher.launch(&request(Role::Coder), None);

        assert!(result.output.is_none());
        assert!(result.spawn_error.as_deref().is_some_and(|error| {
            error.contains("world is missing") || error.contains("world is destroyed")
        }));
    }
}
