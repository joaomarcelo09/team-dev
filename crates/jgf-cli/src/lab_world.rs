//! Lifecycle and configuration for a persistent, isolated lab world.

use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use jgf_core::{AuditKind, AuditLog};
use jgf_hub::ShimAuditEvent;

const LAB_WORK_DIR: &str = "work";

/// The non-root uid used inside a lab world.
pub(crate) const LAB_UID: u32 = 1000;
/// Claude API hosts allowed through the lab's egress proxy.
pub(crate) const CLAUDE_EGRESS_HOSTS: [&str; 1] = ["api.anthropic.com"];
/// Ports allowed through the lab's egress proxy.
pub(crate) const CLAUDE_EGRESS_PORTS: [u16; 1] = [443];

/// State of a lab world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorldStatus {
    /// The namespace is available for work.
    Created,
    /// One or more commands are running in the namespace.
    Running,
    /// The namespace has not been created or has been destroyed.
    Destroyed,
}

/// Policy for retaining a lab world at the end of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndOfLifePolicy {
    /// Destroy the world when the run ends; kill switches always destroy it.
    #[allow(dead_code)]
    DestroyOnKill,
    /// Preserve a failed run for inspection; kill switches still destroy it.
    #[allow(dead_code)]
    KeepOnFailure,
}

/// Outcome used to apply an end-of-life policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunEnd {
    Success,
    Failure,
    Killed,
}

/// Paths and isolation properties owned by a lab world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LabWorldSpec {
    pub state_dir: PathBuf,
    pub lab_dir: PathBuf,
    pub work_tree: PathBuf,
    pub scratch_home: PathBuf,
    pub read_only_git: PathBuf,
    pub uid: u32,
    pub egress_hosts: &'static [&'static str],
    pub egress_ports: &'static [u16],
}

struct LifecycleState {
    status: WorldStatus,
    creating: bool,
    destroying: bool,
    active_runs: usize,
    keep_requested: bool,
    created_once: bool,
    spec: Option<LabWorldSpec>,
}

/// Persistent lifecycle metadata and audit trail for one isolated world.
pub(crate) struct LabWorld {
    state_dir: PathBuf,
    audit_dir: PathBuf,
    lab_id: String,
    policy: EndOfLifePolicy,
    state: Mutex<LifecycleState>,
}

impl LabWorld {
    pub(crate) fn new(
        state_dir: impl Into<PathBuf>,
        audit_dir: impl Into<PathBuf>,
        lab_id: impl Into<String>,
        policy: EndOfLifePolicy,
    ) -> Self {
        Self {
            state_dir: state_dir.into(),
            audit_dir: audit_dir.into(),
            lab_id: lab_id.into(),
            policy,
            state: Mutex::new(LifecycleState {
                status: WorldStatus::Destroyed,
                creating: false,
                destroying: false,
                active_runs: 0,
                keep_requested: false,
                created_once: false,
                spec: None,
            }),
        }
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub(crate) fn status(&self) -> WorldStatus {
        let Ok(state) = self.state.lock() else {
            return WorldStatus::Destroyed;
        };
        if state.active_runs > 0 {
            WorldStatus::Running
        } else {
            state.status
        }
    }

    /// Creates a world and records its boundary configuration.
    pub(crate) fn create<T>(
        &self,
        lab_dir: &Path,
        work_tree: &Path,
        scratch_home: &Path,
        create: impl FnOnce(&LabWorldSpec) -> Result<T, String>,
    ) -> Result<T, String> {
        let spec = match self.make_spec(lab_dir, work_tree, scratch_home) {
            Ok(spec) => spec,
            Err(error) => {
                return match self.audit("lab_world:failure") {
                    Ok(()) => Err(error),
                    Err(audit_error) => Err(format!("{error}; {audit_error}")),
                };
            }
        };
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "lab-world state lock is poisoned".to_string())?;
            if state.status != WorldStatus::Destroyed || state.creating || state.destroying {
                return Err("lab world is already created or changing state".to_string());
            }
            state.creating = true;
        }

        if let Err(error) = self.audit("lab_world:create") {
            self.reset_create();
            return Err(error);
        }

        match create(&spec) {
            Ok(value) => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "lab-world state lock is poisoned".to_string())?;
                state.status = WorldStatus::Created;
                state.creating = false;
                state.keep_requested = false;
                state.created_once = true;
                state.spec = Some(spec);
                Ok(value)
            }
            Err(error) => {
                self.reset_create();
                match self.audit("lab_world:failure") {
                    Ok(()) => Err(error),
                    Err(audit_error) => Err(format!("{error}; {audit_error}")),
                }
            }
        }
    }

    /// Marks the world as running a command.
    pub(crate) fn start_run(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "lab-world state lock is poisoned".to_string())?;
        if state.status == WorldStatus::Destroyed || state.creating || state.destroying {
            return Err("lab world is not available to run".to_string());
        }
        if state.active_runs == 0 {
            self.audit("lab_world:running")?;
        }
        state.active_runs = state.active_runs.saturating_add(1);
        Ok(())
    }

    /// Marks one command as finished.
    pub(crate) fn end_run(&self) {
        if let Ok(mut state) = self.state.lock()
            && state.active_runs > 0
        {
            state.active_runs -= 1;
        }
    }

    /// Requests that this world and its state be left on disk.
    pub(crate) fn keep(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "lab-world state lock is poisoned".to_string())?;
        if state.status == WorldStatus::Destroyed || state.creating || state.destroying {
            return Err("cannot keep a lab world that is not available".to_string());
        }
        if state.keep_requested {
            return Ok(());
        }
        let audit_error = self.audit("lab_world:keep").err();
        state.keep_requested = true;
        audit_error.map_or(Ok(()), Err)
    }

    /// Records a kill-switch event.
    pub(crate) fn record_kill(&self) -> Result<(), String> {
        self.audit("lab_world:kill")
    }

    /// Records that the run failed.
    pub(crate) fn record_failure(&self) -> Result<(), String> {
        self.audit("lab_world:failure")
    }

    /// Applies the configured policy, with an explicit keep request taking precedence over
    /// success or failure. A kill always destroys the world.
    pub(crate) fn finish(
        &self,
        outcome: RunEnd,
        destroy: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        match outcome {
            RunEnd::Killed => {
                let should_record_kill = self
                    .state
                    .lock()
                    .map(|state| state.status != WorldStatus::Destroyed || !state.created_once)
                    .map_err(|_| "lab-world state lock is poisoned".to_string())?;
                let kill_audit_error = if should_record_kill {
                    self.record_kill().err()
                } else {
                    None
                };
                combine_errors(self.destroy(destroy), kill_audit_error)
            }
            RunEnd::Success => {
                if self.keep_requested()? {
                    Ok(())
                } else {
                    self.destroy(destroy)
                }
            }
            RunEnd::Failure => {
                let failure_audit_error = self.record_failure().err();
                let status = self.status();
                let result = if status == WorldStatus::Destroyed || self.keep_requested()? {
                    Ok(())
                } else {
                    match self.policy {
                        EndOfLifePolicy::KeepOnFailure => self.keep(),
                        EndOfLifePolicy::DestroyOnKill => self.destroy(destroy),
                    }
                };
                combine_errors(result, failure_audit_error)
            }
        }
    }

    /// Destroys the world. Repeated calls are safe.
    pub(crate) fn destroy(
        &self,
        destroy: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "lab-world state lock is poisoned".to_string())?;
            if state.status == WorldStatus::Destroyed {
                return Ok(());
            }
            if state.creating || state.destroying {
                return Err("lab world is already changing state".to_string());
            }
            state.destroying = true;
        }

        let audit_error = self.audit("lab_world:destroy").err();

        match destroy() {
            Ok(()) => {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "lab-world state lock is poisoned".to_string())?;
                state.status = WorldStatus::Destroyed;
                state.destroying = false;
                state.active_runs = 0;
                state.keep_requested = false;
                state.spec = None;
                audit_error.map_or(Ok(()), Err)
            }
            Err(error) => {
                self.reset_destroy();
                let failure_audit_error = self.audit("lab_world:failure").err();
                combine_errors(Err(error), failure_audit_error.or(audit_error))
            }
        }
    }

    /// Drop-time cleanup respects an explicit keep request.
    pub(crate) fn destroy_if_not_kept(
        &self,
        destroy: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        if self.keep_requested()? {
            return Ok(());
        }
        self.destroy(destroy)
    }

    fn make_spec(
        &self,
        lab_dir: &Path,
        work_tree: &Path,
        scratch_home: &Path,
    ) -> Result<LabWorldSpec, String> {
        if work_tree != lab_dir.join(LAB_WORK_DIR) {
            return Err("Claude work tree must be the lab's work directory".to_string());
        }
        if scratch_home.starts_with(work_tree) || work_tree.starts_with(scratch_home) {
            return Err("lab scratch HOME must be outside the work tree".to_string());
        }
        if self.state_dir.starts_with(work_tree) || work_tree.starts_with(&self.state_dir) {
            return Err("lab state directory must be separate from the work tree".to_string());
        }
        if self.state_dir.starts_with(scratch_home) || scratch_home.starts_with(&self.state_dir) {
            return Err("lab state directory must be separate from scratch HOME".to_string());
        }
        Ok(LabWorldSpec {
            state_dir: self.state_dir.clone(),
            lab_dir: lab_dir.to_path_buf(),
            work_tree: work_tree.to_path_buf(),
            scratch_home: scratch_home.to_path_buf(),
            read_only_git: work_tree.join(".git"),
            uid: LAB_UID,
            egress_hosts: &CLAUDE_EGRESS_HOSTS,
            egress_ports: &CLAUDE_EGRESS_PORTS,
        })
    }

    fn keep_requested(&self) -> Result<bool, String> {
        self.state
            .lock()
            .map(|state| state.keep_requested)
            .map_err(|_| "lab-world state lock is poisoned".to_string())
    }

    fn audit(&self, message: &str) -> Result<(), String> {
        self.audit_tagged(None, None, message)
    }

    pub(crate) fn record_role_start(
        &self,
        project: &str,
        worker: &str,
        role: &str,
    ) -> Result<(), String> {
        self.audit_tagged(Some(project), Some(worker), &format!("role_start:{role}"))
    }

    pub(crate) fn record_hub_event(&self, event: &ShimAuditEvent) -> Result<(), String> {
        self.audit(&event.audit_summary())
    }

    fn audit_tagged(
        &self,
        project: Option<&str>,
        worker: Option<&str>,
        message: &str,
    ) -> Result<(), String> {
        let log = AuditLog::open(&self.audit_dir, self.lab_id.clone())
            .map_err(|error| format!("could not open lab audit log: {error}"))?;
        log.append(project, worker, AuditKind::note(message))
            .map(|_| ())
            .map_err(|error| format!("could not append lab lifecycle audit event: {error}"))
    }

    fn reset_create(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.creating = false;
        }
    }

    fn reset_destroy(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.destroying = false;
        }
    }
}

fn combine_errors(result: Result<(), String>, additional: Option<String>) -> Result<(), String> {
    match (result, additional) {
        (Ok(()), None) => Ok(()),
        (Ok(()), Some(error)) | (Err(error), None) => Err(error),
        (Err(error), Some(additional)) => Err(format!("{error}; {additional}")),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;

    fn fixture(root: &Path, policy: EndOfLifePolicy) -> (LabWorld, PathBuf, PathBuf, PathBuf) {
        let lab_dir = root.join("lab");
        let work_tree = lab_dir.join(LAB_WORK_DIR);
        let scratch = lab_dir.join("scratch");
        fs::create_dir_all(&work_tree).expect("work tree");
        fs::create_dir_all(&scratch).expect("scratch home");
        let world = LabWorld::new(root.join("state"), root.join("audit"), "test-lab", policy);
        (world, lab_dir, work_tree, scratch)
    }

    fn create(world: &LabWorld, lab_dir: &Path, work_tree: &Path, scratch: &Path) -> LabWorldSpec {
        world
            .create(lab_dir, work_tree, scratch, |spec| {
                assert_eq!(spec.uid, 1000);
                assert_eq!(spec.egress_hosts, ["api.anthropic.com"]);
                assert_eq!(spec.egress_ports, [443]);
                assert_eq!(spec.read_only_git, work_tree.join(".git"));
                Ok(spec.clone())
            })
            .expect("create world")
    }

    fn audit_tags(root: &Path) -> Vec<String> {
        AuditLog::open(root, "test-lab")
            .expect("audit log")
            .read_all()
            .expect("audit events")
            .into_iter()
            .map(|event| {
                assert_eq!(event.lab, "test-lab");
                match event.kind {
                    AuditKind::Note { text } => text,
                    other => panic!("unexpected lifecycle event: {other:?}"),
                }
            })
            .collect()
    }

    #[test]
    fn role_start_audit_records_lab_project_and_worker_once() {
        let temp = tempfile::tempdir().expect("temp dir");
        let world = LabWorld::new(
            temp.path().join("state"),
            temp.path().join("audit"),
            "test-lab",
            EndOfLifePolicy::KeepOnFailure,
        );

        world
            .record_role_start("project-a", "worker-1", "coder")
            .expect("role start audit");

        let events = AuditLog::open(temp.path().join("audit"), "test-lab")
            .expect("audit log")
            .read_all()
            .expect("audit events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].lab, "test-lab");
        assert_eq!(events[0].project.as_deref(), Some("project-a"));
        assert_eq!(events[0].worker.as_deref(), Some("worker-1"));
        assert_eq!(events[0].kind, AuditKind::note("role_start:coder"));
    }

    #[test]
    fn create_sets_configuration_and_status_and_audits_the_lab_id() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::DestroyOnKill);

        let spec = create(&world, &lab_dir, &work_tree, &scratch);

        assert_eq!(world.status(), WorldStatus::Created);
        assert_eq!(spec.state_dir, temp.path().join("state"));
        assert_eq!(spec.scratch_home, scratch);
        assert_eq!(audit_tags(&temp.path().join("audit")), ["lab_world:create"]);
    }

    #[test]
    fn create_failure_is_audited_without_marking_the_world_created() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::DestroyOnKill);

        let result = world.create::<()>(&lab_dir, &work_tree, &scratch, |_| {
            Err("fake create failure".to_string())
        });

        assert!(result.is_err());
        assert_eq!(world.status(), WorldStatus::Destroyed);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:failure"]
        );
    }

    #[test]
    fn a_failed_run_before_world_creation_does_not_attempt_to_keep_or_destroy() {
        let temp = tempfile::tempdir().expect("temp dir");
        let world = LabWorld::new(
            temp.path().join("state"),
            temp.path().join("audit"),
            "test-lab",
            EndOfLifePolicy::KeepOnFailure,
        );

        world
            .finish(RunEnd::Failure, || panic!("there is no world to destroy"))
            .expect("finish setup failure");

        assert_eq!(world.status(), WorldStatus::Destroyed);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:failure"]
        );
    }

    #[test]
    fn running_status_returns_to_created_when_a_command_finishes() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::DestroyOnKill);
        create(&world, &lab_dir, &work_tree, &scratch);

        world.start_run().expect("start command");
        assert_eq!(world.status(), WorldStatus::Running);
        world.end_run();

        assert_eq!(world.status(), WorldStatus::Created);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:running"]
        );
    }

    #[test]
    fn keep_prevents_drop_cleanup_but_explicit_destroy_remains_available() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::DestroyOnKill);
        create(&world, &lab_dir, &work_tree, &scratch);
        let destroys = Arc::new(AtomicUsize::new(0));

        world.keep().expect("keep world");
        let drop_destroys = Arc::clone(&destroys);
        world
            .destroy_if_not_kept(move || {
                drop_destroys.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .expect("drop cleanup");

        assert_eq!(destroys.load(Ordering::SeqCst), 0);
        assert_eq!(world.status(), WorldStatus::Created);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:keep"]
        );
        world.destroy(|| Ok(())).expect("explicit destroy");
        assert_eq!(world.status(), WorldStatus::Destroyed);
    }

    #[test]
    fn destroy_is_idempotent_and_audits_once() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::DestroyOnKill);
        create(&world, &lab_dir, &work_tree, &scratch);
        let destroys = Arc::new(AtomicUsize::new(0));

        for _ in 0..2 {
            let destroys = Arc::clone(&destroys);
            world
                .destroy(move || {
                    destroys.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .expect("destroy world");
        }

        assert_eq!(destroys.load(Ordering::SeqCst), 1);
        assert_eq!(world.status(), WorldStatus::Destroyed);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:destroy"]
        );
    }

    #[test]
    fn kill_always_destroys_even_when_failure_policy_keeps_worlds() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::KeepOnFailure);
        create(&world, &lab_dir, &work_tree, &scratch);

        world
            .finish(RunEnd::Killed, || Ok(()))
            .expect("finish killed run");

        assert_eq!(world.status(), WorldStatus::Destroyed);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:kill", "lab_world:destroy"]
        );
    }

    #[test]
    fn successful_run_destroys_the_world_by_default() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::KeepOnFailure);
        create(&world, &lab_dir, &work_tree, &scratch);
        let destroys = Arc::new(AtomicUsize::new(0));
        let destroy_counter = Arc::clone(&destroys);

        world
            .finish(RunEnd::Success, move || {
                destroy_counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .expect("finish successful run");

        assert_eq!(destroys.load(Ordering::SeqCst), 1);
        assert_eq!(world.status(), WorldStatus::Destroyed);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:destroy"]
        );
    }

    #[test]
    fn failed_run_is_kept_under_keep_on_failure_policy() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::KeepOnFailure);
        create(&world, &lab_dir, &work_tree, &scratch);

        world
            .finish(RunEnd::Failure, || panic!("failed runs are retained"))
            .expect("finish failed run");

        assert_eq!(world.status(), WorldStatus::Created);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:failure", "lab_world:keep"]
        );
    }

    #[test]
    fn failed_run_is_destroyed_under_destroy_on_kill_policy() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::DestroyOnKill);
        create(&world, &lab_dir, &work_tree, &scratch);

        world
            .finish(RunEnd::Failure, || Ok(()))
            .expect("finish failed run");

        assert_eq!(world.status(), WorldStatus::Destroyed);
        assert_eq!(
            audit_tags(&temp.path().join("audit")),
            ["lab_world:create", "lab_world:failure", "lab_world:destroy"]
        );
    }

    #[test]
    fn successful_run_destroys_unless_keep_was_requested() {
        let temp = tempfile::tempdir().expect("temp dir");
        let (world, lab_dir, work_tree, scratch) =
            fixture(temp.path(), EndOfLifePolicy::KeepOnFailure);
        create(&world, &lab_dir, &work_tree, &scratch);
        world.keep().expect("keep world");

        world
            .finish(RunEnd::Success, || panic!("explicit keep wins"))
            .expect("finish successful run");

        assert_eq!(world.status(), WorldStatus::Created);
    }
}
