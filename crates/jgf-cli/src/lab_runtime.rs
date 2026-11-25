//! Scheduler-gated launch of coder and reviewer roles in a lab.

use std::path::PathBuf;

use jgf_core::{AuditKind, AuditLog, ModelError, ProjectId};
use jgf_hub::{
    CoderGrant, CoderSlot, ReviewTicket, Scheduler, SchedulerError, SlotId, Switch, WaitReason,
};
use thiserror::Error;

use crate::{
    e2e::KillSwitch,
    roles::{Role, RoleLaunch, RoleLauncher},
    sandbox::ChildRun,
};

/// A launcher used by [`LabRuntime`].
///
/// This small interface also lets tests exercise scheduler decisions without starting processes.
pub trait RoleLaunchService {
    /// Starts the prepared role and waits for its child to finish.
    fn launch(&self, request: &RoleLaunch, kill: Option<&dyn KillSwitch>) -> ChildRun;

    /// Starts a session-persistent role, optionally resuming the supplied Claude session.
    fn launch_with_session(
        &self,
        request: &RoleLaunch,
        resume_session: Option<&str>,
        kill: Option<&dyn KillSwitch>,
    ) -> ChildRun {
        let _ = resume_session;
        self.launch(request, kill)
    }
}

impl RoleLaunchService for RoleLauncher<'_> {
    fn launch(&self, request: &RoleLaunch, kill: Option<&dyn KillSwitch>) -> ChildRun {
        RoleLauncher::launch(self, request, kill)
    }

    fn launch_with_session(
        &self,
        request: &RoleLaunch,
        resume_session: Option<&str>,
        kill: Option<&dyn KillSwitch>,
    ) -> ChildRun {
        RoleLauncher::launch_with_session(self, request, resume_session, kill)
    }
}

/// Why a launch is waiting for a scheduler decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewWaitReason {
    /// A review already owns the lab-wide review slot.
    ReviewRunning,
    /// An earlier queued review is eligible first.
    EarlierReview,
    /// This review's project still has a coder running.
    ProjectCoderRunning,
}

/// Outcome of requesting a coder launch.
#[derive(Debug)]
pub enum CoderLaunch {
    /// No process was started; retry after the scheduler state changes.
    Wait(WaitReason),
    /// The process ended and its scheduler slot has been released.
    Finished(ChildRun),
}

/// Outcome of requesting a reviewer launch.
#[derive(Debug)]
pub enum ReviewerLaunch {
    /// No process was started; the review remains queued when it was accepted.
    Wait(ReviewWaitReason),
    /// The process ended and its scheduler review ticket has been finished.
    Finished(ChildRun),
}

/// Slots returned by a kill, plus any audit append failure.
#[derive(Debug)]
pub struct KillOutcome {
    /// Running coder slots that the caller should stop through its existing kill switch.
    pub slots: Vec<CoderSlot>,
    /// Audit failure, if the scheduler was killed but its event could not be written.
    pub audit_error: Option<ModelError>,
}

/// Errors raised before a role can be run or while recording scheduler events.
#[derive(Debug, Error)]
pub enum LabRuntimeError {
    /// The lab is paused and cannot grant new work.
    #[error("the lab is paused")]
    Paused,
    /// The lab has been killed and cannot grant new work.
    #[error("the lab is killed")]
    Killed,
    /// The requested role does not match the scheduler operation.
    #[error("expected a {expected} role launch")]
    WrongRole {
        /// Required role name.
        expected: &'static str,
    },
    /// The scheduler has not granted this project a team slot.
    #[error("the scheduler has not granted project {0}")]
    ProjectNotGranted(String),
    /// The launch request does not contain a valid project identifier.
    #[error("invalid project identifier: {0}")]
    InvalidProject(#[source] ModelError),
    /// A scheduler operation failed for a reason other than pause or kill.
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    /// The runtime could not write its audit event.
    #[error("could not write a lab runtime audit event: {0}")]
    Audit(#[from] ModelError),
    /// The scheduler started a review other than the request this caller can launch.
    #[error("the scheduler started a different review")]
    UnexpectedReview,
}

/// The lab scheduler, role launcher and tagged audit log used to gate role starts.
pub struct LabRuntime<L> {
    scheduler: Scheduler,
    launcher: L,
    audit: AuditLog,
}

impl<L: RoleLaunchService> LabRuntime<L> {
    /// Creates a runtime with the scheduler's existing persisted state and lab audit log.
    pub fn new(
        scheduler: Scheduler,
        launcher: L,
        audit_dir: impl Into<PathBuf>,
        lab_id: impl Into<String>,
    ) -> Result<Self, LabRuntimeError> {
        let audit = AuditLog::open(audit_dir, lab_id)?;
        Ok(Self {
            scheduler,
            launcher,
            audit,
        })
    }

    /// The scheduler can be configured by the lab coordinator before launches begin.
    pub fn scheduler(&self) -> &Scheduler {
        &self.scheduler
    }

    /// Starts a Lab Coordinator turn only while the lab scheduler allows new work.
    pub fn launch_coordinator(
        &self,
        request: &RoleLaunch,
        resume_session: Option<&str>,
        kill: Option<&dyn KillSwitch>,
    ) -> Result<ChildRun, LabRuntimeError> {
        if request.role != Role::LabCoordinator {
            return Err(LabRuntimeError::WrongRole {
                expected: "lab coordinator",
            });
        }
        let switch = self.scheduler.switch()?;
        if switch != Switch::Running {
            let error = match switch {
                Switch::Paused => SchedulerError::Paused,
                Switch::Killed => SchedulerError::Killed,
                Switch::Running => unreachable!("switch checked above"),
            };
            return Err(self.map_scheduler_error(
                error,
                "coordinator_launch",
                Some(&request.project_id),
                Some(&request.worker_id),
            )?);
        }
        Ok(self
            .launcher
            .launch_with_session(request, resume_session, kill))
    }

    /// Starts a Co-coordinator turn only for a project granted by the scheduler.
    pub fn launch_co_coordinator(
        &self,
        request: &RoleLaunch,
        project_id: &str,
        resume_session: Option<&str>,
        kill: Option<&dyn KillSwitch>,
    ) -> Result<ChildRun, LabRuntimeError> {
        if request.role != Role::CoCoordinator || request.project_id != project_id {
            return Err(LabRuntimeError::WrongRole {
                expected: "co-coordinator",
            });
        }
        let project = ProjectId::parse(project_id).map_err(LabRuntimeError::InvalidProject)?;
        match self.scheduler.switch()? {
            Switch::Running => {}
            Switch::Paused => return Err(LabRuntimeError::Paused),
            Switch::Killed => return Err(LabRuntimeError::Killed),
        }
        let has_slot = self
            .scheduler
            .coder_slots()?
            .iter()
            .any(|slot| slot.project == project);
        if !has_slot && self.scheduler.next_to_start()?.as_ref() != Some(&project) {
            return Err(LabRuntimeError::ProjectNotGranted(project_id.to_string()));
        }
        Ok(self
            .launcher
            .launch_with_session(request, resume_session, kill))
    }

    /// Acquires a coder slot before invoking the launcher.
    ///
    /// A wait result does not start a child. Every granted slot is released when the synchronous
    /// launcher returns, including failed and killed child runs.
    pub fn launch_coder(
        &self,
        request: &RoleLaunch,
        repo: impl Into<String>,
        kill: Option<&dyn KillSwitch>,
    ) -> Result<CoderLaunch, LabRuntimeError> {
        if request.role != Role::Coder {
            return Err(LabRuntimeError::WrongRole { expected: "coder" });
        }
        let project =
            ProjectId::parse(&request.project_id).map_err(LabRuntimeError::InvalidProject)?;
        let project_tag = project.as_str().to_string();
        let slot = match self.scheduler.acquire_coder(project, repo) {
            Ok(CoderGrant::Granted(slot)) => slot,
            Ok(CoderGrant::Wait(reason)) => {
                self.record(
                    Some(&project_tag),
                    Some(&request.worker_id),
                    &format!("lab_runtime:coder_wait:{}", coder_wait_name(reason)),
                )?;
                return Ok(CoderLaunch::Wait(reason));
            }
            Err(error) => {
                return Err(self.map_scheduler_error(
                    error,
                    "coder_wait",
                    Some(&project_tag),
                    Some(&request.worker_id),
                )?);
            }
        };

        let mut guard = CoderSlotGuard::new(
            &self.scheduler,
            &self.audit,
            slot,
            request.worker_id.clone(),
        );
        self.record(
            Some(&project_tag),
            Some(&request.worker_id),
            &format!("lab_runtime:coder_granted:{}", guard.slot_id()),
        )?;
        let child = self.launcher.launch(request, kill);
        guard.release()?;
        Ok(CoderLaunch::Finished(child))
    }

    /// Queues and starts a reviewer only when it is the next eligible FIFO ticket.
    ///
    /// A waiting review stays queued and can be retried by calling this method again with the
    /// same project and task. Duplicate retries reuse the existing queued ticket.
    pub fn launch_reviewer(
        &self,
        request: &RoleLaunch,
        task: impl Into<String>,
        kill: Option<&dyn KillSwitch>,
    ) -> Result<ReviewerLaunch, LabRuntimeError> {
        self.launch_reviewer_with_hook(request, task, kill, |_| {})
    }

    fn launch_reviewer_with_hook(
        &self,
        request: &RoleLaunch,
        task: impl Into<String>,
        kill: Option<&dyn KillSwitch>,
        before_start: impl FnOnce(&Scheduler),
    ) -> Result<ReviewerLaunch, LabRuntimeError> {
        if request.role != Role::Reviewer {
            return Err(LabRuntimeError::WrongRole {
                expected: "reviewer",
            });
        }
        let project =
            ProjectId::parse(&request.project_id).map_err(LabRuntimeError::InvalidProject)?;
        let project_tag = project.as_str().to_string();
        let ticket = ReviewTicket {
            project,
            task: task.into(),
        };
        let switch = self.scheduler.switch()?;
        if switch != Switch::Running {
            let error = match switch {
                Switch::Paused => SchedulerError::Paused,
                Switch::Killed => SchedulerError::Killed,
                Switch::Running => unreachable!("switch checked above"),
            };
            return Err(self.map_scheduler_error(
                error,
                "review_wait",
                Some(&project_tag),
                Some(&request.worker_id),
            )?);
        }

        let queued = self.scheduler.review_queue()?;
        let running = self.scheduler.running_review()?;
        if !queued.contains(&ticket)
            && running.as_ref() != Some(&ticket)
            && let Err(error) = self
                .scheduler
                .request_review(ticket.project.clone(), ticket.task.clone())
        {
            return Err(self.map_scheduler_error(
                error,
                "review_wait",
                Some(&project_tag),
                Some(&request.worker_id),
            )?);
        }

        if running.is_some() {
            return self.wait_for_review(
                ReviewWaitReason::ReviewRunning,
                &project_tag,
                &request.worker_id,
            );
        }

        let queue = self.scheduler.review_queue()?;
        let coder_slots = self.scheduler.coder_slots()?;
        let selected = queue.iter().find(|queued_ticket| {
            !coder_slots
                .iter()
                .any(|slot| slot.project == queued_ticket.project)
        });
        if selected != Some(&ticket) {
            let reason = if coder_slots
                .iter()
                .any(|slot| slot.project == ticket.project)
            {
                ReviewWaitReason::ProjectCoderRunning
            } else {
                ReviewWaitReason::EarlierReview
            };
            return self.wait_for_review(reason, &project_tag, &request.worker_id);
        }

        before_start(&self.scheduler);
        match self.scheduler.start_review_if_next(&ticket) {
            Ok(true) => {}
            Ok(false) => {
                let switch = self.scheduler.switch()?;
                if switch != Switch::Running {
                    let error = match switch {
                        Switch::Paused => SchedulerError::Paused,
                        Switch::Killed => SchedulerError::Killed,
                        Switch::Running => unreachable!("switch checked above"),
                    };
                    return Err(self.map_scheduler_error(
                        error,
                        "review_wait",
                        Some(&project_tag),
                        Some(&request.worker_id),
                    )?);
                }
                let reason = self.review_wait_reason(&ticket)?;
                return self.wait_for_review(reason, &project_tag, &request.worker_id);
            }
            Err(error) => {
                return Err(self.map_scheduler_error(
                    error,
                    "review_wait",
                    Some(&project_tag),
                    Some(&request.worker_id),
                )?);
            }
        }

        let mut guard = ReviewGuard::new(
            &self.scheduler,
            &self.audit,
            ticket,
            request.worker_id.clone(),
        );
        self.record(
            Some(&project_tag),
            Some(&request.worker_id),
            "lab_runtime:review_started",
        )?;
        let child = self.launcher.launch(request, kill);
        guard.finish()?;
        Ok(ReviewerLaunch::Finished(child))
    }

    /// Kills the scheduler and returns the active coder slots for stopping through the caller's
    /// existing [`KillSwitch`]. Slots remain occupied until their child guards release them.
    pub fn kill(&self) -> Result<KillOutcome, LabRuntimeError> {
        let slots = self.scheduler.kill()?;
        let audit_error = self.record(None, None, "lab_runtime:killed").err();
        Ok(KillOutcome { slots, audit_error })
    }

    fn review_wait_reason(
        &self,
        ticket: &ReviewTicket,
    ) -> Result<ReviewWaitReason, LabRuntimeError> {
        if self.scheduler.running_review()?.is_some() {
            return Ok(ReviewWaitReason::ReviewRunning);
        }
        if self
            .scheduler
            .coder_slots()?
            .iter()
            .any(|slot| slot.project == ticket.project)
        {
            return Ok(ReviewWaitReason::ProjectCoderRunning);
        }
        Ok(ReviewWaitReason::EarlierReview)
    }

    fn wait_for_review(
        &self,
        reason: ReviewWaitReason,
        project: &str,
        worker: &str,
    ) -> Result<ReviewerLaunch, LabRuntimeError> {
        self.record(
            Some(project),
            Some(worker),
            &format!("lab_runtime:review_wait:{}", review_wait_name(reason)),
        )?;
        Ok(ReviewerLaunch::Wait(reason))
    }

    fn record(
        &self,
        project: Option<&str>,
        worker: Option<&str>,
        message: &str,
    ) -> Result<(), ModelError> {
        self.audit
            .append(project, worker, AuditKind::note(message))
            .map(|_| ())
    }

    fn map_scheduler_error(
        &self,
        error: SchedulerError,
        operation: &str,
        project: Option<&str>,
        worker: Option<&str>,
    ) -> Result<LabRuntimeError, LabRuntimeError> {
        match error {
            SchedulerError::Paused => {
                self.record(project, worker, &format!("lab_runtime:{operation}:paused"))?;
                Ok(LabRuntimeError::Paused)
            }
            SchedulerError::Killed => {
                self.record(project, worker, &format!("lab_runtime:{operation}:killed"))?;
                Ok(LabRuntimeError::Killed)
            }
            other => Ok(LabRuntimeError::Scheduler(other)),
        }
    }
}

fn coder_wait_name(reason: WaitReason) -> &'static str {
    match reason {
        WaitReason::TeamLimit => "team_limit",
        WaitReason::CoderLimit => "coder_limit",
        WaitReason::ProjectAlreadyRunning => "project_already_running",
        WaitReason::OtherProjectRunning => "other_project_running",
        WaitReason::RepositoryBusy => "repository_busy",
    }
}

fn review_wait_name(reason: ReviewWaitReason) -> &'static str {
    match reason {
        ReviewWaitReason::ReviewRunning => "review_running",
        ReviewWaitReason::EarlierReview => "earlier_review",
        ReviewWaitReason::ProjectCoderRunning => "project_coder_running",
    }
}

struct CoderSlotGuard<'a> {
    scheduler: &'a Scheduler,
    audit: &'a AuditLog,
    slot: Option<CoderSlot>,
    worker: String,
}

impl<'a> CoderSlotGuard<'a> {
    fn new(scheduler: &'a Scheduler, audit: &'a AuditLog, slot: CoderSlot, worker: String) -> Self {
        Self {
            scheduler,
            audit,
            slot: Some(slot),
            worker,
        }
    }

    fn slot_id(&self) -> SlotId {
        self.slot.as_ref().expect("coder slot guard is active").id
    }

    fn release(&mut self) -> Result<(), LabRuntimeError> {
        let Some(slot) = self.slot.take() else {
            return Ok(());
        };
        let released = self.scheduler.release(slot.id)?;
        self.audit
            .append(
                Some(released.project.as_str()),
                Some(&self.worker),
                AuditKind::note(format!("lab_runtime:coder_released:{}", released.id)),
            )
            .map(|_| ())
            .map_err(LabRuntimeError::Audit)
    }
}

impl Drop for CoderSlotGuard<'_> {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

struct ReviewGuard<'a> {
    scheduler: &'a Scheduler,
    audit: &'a AuditLog,
    ticket: Option<ReviewTicket>,
    worker: String,
}

impl<'a> ReviewGuard<'a> {
    fn new(
        scheduler: &'a Scheduler,
        audit: &'a AuditLog,
        ticket: ReviewTicket,
        worker: String,
    ) -> Self {
        Self {
            scheduler,
            audit,
            ticket: Some(ticket),
            worker,
        }
    }

    fn finish(&mut self) -> Result<(), LabRuntimeError> {
        let Some(ticket) = self.ticket.take() else {
            return Ok(());
        };
        let finished = self
            .scheduler
            .finish_review()?
            .ok_or(LabRuntimeError::UnexpectedReview)?;
        if finished != ticket {
            return Err(LabRuntimeError::UnexpectedReview);
        }
        self.audit
            .append(
                Some(finished.project.as_str()),
                Some(&self.worker),
                AuditKind::note("lab_runtime:review_finished"),
            )
            .map(|_| ())
            .map_err(LabRuntimeError::Audit)
    }
}

impl Drop for ReviewGuard<'_> {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        path::Path,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use jgf_core::{AuditKind, AuditLog, EnvProfile, Project, ProjectId, ProjectStage};
    use jgf_hub::{CoderGrant, CoderSlot, Mode, ReviewTicket, Scheduler, WaitReason};
    use tempfile::TempDir;

    use super::{
        CoderLaunch, LabRuntime, LabRuntimeError, ReviewWaitReason, ReviewerLaunch,
        RoleLaunchService,
    };
    use crate::{
        e2e::KillSwitch,
        roles::{Role, RoleLaunch, RoleLauncher},
        sandbox::{ChildRun, ClaudeSandbox},
    };

    #[derive(Default)]
    struct FakeLauncher {
        calls: AtomicUsize,
        runs: Mutex<VecDeque<ChildRun>>,
        coder_before_review: Mutex<Option<ProjectId>>,
        acquired_coder: Mutex<Option<CoderSlot>>,
    }

    impl FakeLauncher {
        fn with_runs(runs: impl IntoIterator<Item = ChildRun>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                runs: Mutex::new(runs.into_iter().collect()),
                ..Self::default()
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn acquire_coder_before_review(&self, project: ProjectId) {
            *self.coder_before_review.lock().expect("fake hook lock") = Some(project);
        }

        fn run_review_start_hook(&self, scheduler: &Scheduler) {
            let Some(project) = self
                .coder_before_review
                .lock()
                .expect("fake hook lock")
                .take()
            else {
                return;
            };
            let slot = match scheduler
                .acquire_coder(project, "backend")
                .expect("acquire fake coder")
            {
                CoderGrant::Granted(slot) => slot,
                CoderGrant::Wait(reason) => panic!("unexpected fake coder wait: {reason:?}"),
            };
            *self.acquired_coder.lock().expect("fake coder lock") = Some(slot);
        }

        fn take_acquired_coder(&self) -> Option<CoderSlot> {
            self.acquired_coder.lock().expect("fake coder lock").take()
        }
    }

    impl RoleLaunchService for FakeLauncher {
        fn launch(&self, _request: &RoleLaunch, _kill: Option<&dyn KillSwitch>) -> ChildRun {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.runs
                .lock()
                .expect("fake launcher lock")
                .pop_front()
                .unwrap_or_default()
        }
    }

    fn fixture(mode: Mode, launcher: FakeLauncher) -> (TempDir, LabRuntime<FakeLauncher>) {
        let temp = TempDir::new().expect("temporary directory");
        let scheduler = Scheduler::open(temp.path().join("scheduler")).expect("scheduler");
        scheduler.set_mode(mode).expect("set mode");
        let runtime = LabRuntime::new(scheduler, launcher, temp.path().join("audit"), "test-lab")
            .expect("runtime");
        (temp, runtime)
    }

    fn request(role: Role, project: &str, worker: &str) -> RoleLaunch {
        RoleLaunch {
            role,
            project_id: project.to_string(),
            worker_id: worker.to_string(),
            worktree_name: format!("{project}-work"),
            prompt: "test".to_string(),
            timeout: Duration::from_secs(1),
            max_turns: 1,
            max_budget_usd: 1.0,
        }
    }

    fn project(raw: &str) -> ProjectId {
        ProjectId::parse(raw).expect("project")
    }

    fn note_events(root: &TempDir) -> Vec<AuditKind> {
        AuditLog::open(root.path().join("audit"), "test-lab")
            .expect("audit")
            .read_all()
            .expect("audit events")
            .into_iter()
            .map(|event| event.kind)
            .collect()
    }

    #[test]
    #[ignore = "requires the real Claude CLI, configured credentials, and small API requests"]
    fn real_claude_roles_run_through_the_lab_runtime() {
        let temp = TempDir::new().expect("temporary directory");
        let lab_dir = temp.path().join("lab");
        let lab_work_tree = lab_dir.join("work");
        fs::create_dir_all(lab_work_tree.join(".git/info")).expect("lab git info directory");
        fs::write(lab_work_tree.join(".git/info/exclude"), "").expect("lab git exclude");

        let project = Project {
            id: project("smoke-project"),
            name: "Runtime smoke test".to_string(),
            spec_path: temp.path().join("spec.md"),
            spec_sha256: None,
            repos: Vec::new(),
            stage: ProjectStage::Mvp,
            env_profile: EnvProfile::default(),
            recipes: Vec::new(),
        };
        let sandbox_state = temp.path().join("sandbox-state");
        jgf_core::registry::save_project(&sandbox_state, &project).expect("save project");

        let worktree_name = format!("{}-work", project.id.as_str());
        let project_work_tree = lab_dir
            .join("projects")
            .join(project.id.as_str())
            .join("worktrees")
            .join(&worktree_name);
        fs::create_dir_all(&project_work_tree).expect("project worktree");

        let sandbox = ClaudeSandbox::new(&sandbox_state, "claude");
        sandbox
            .create(&lab_dir, &lab_work_tree)
            .unwrap_or_else(|error| panic!("jg-sandbox setup failed: {error}"));
        let skills_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("factory-skills");
        let launcher = RoleLauncher::new(skills_dir, &sandbox);
        let scheduler = Scheduler::open(temp.path().join("scheduler")).expect("scheduler");
        scheduler.set_mode(Mode::A).expect("set scheduler mode");
        let runtime = LabRuntime::new(scheduler, launcher, temp.path().join("audit"), "smoke-lab")
            .expect("runtime");

        let mut coder = request(Role::Coder, project.id.as_str(), "smoke-coder");
        coder.prompt = "Reply with exactly OK. Do not use tools.".to_string();
        coder.timeout = Duration::from_secs(120);
        coder.max_turns = 1;
        coder.max_budget_usd = 0.25;
        let coder_run = match runtime
            .launch_coder(&coder, "workspace", None)
            .expect("launch coder")
        {
            CoderLaunch::Finished(run) => run,
            CoderLaunch::Wait(reason) => panic!("coder unexpectedly waited: {reason:?}"),
        };
        assert!(
            coder_run.spawn_error.is_none(),
            "{}",
            coder_run.spawn_error.unwrap_or_default()
        );
        let coder_output = coder_run.output.expect("coder output");
        assert_eq!(
            coder_output.exit_code,
            Some(0),
            "{}",
            coder_output.stderr_text()
        );
        assert!(coder_output.stdout_text().contains("OK"));
        assert!(
            runtime
                .scheduler()
                .coder_slots()
                .expect("coder slots")
                .is_empty(),
            "coder slot should be released after the coder exits"
        );
        let runtime_notes = AuditLog::open(temp.path().join("audit"), "smoke-lab")
            .expect("runtime audit")
            .read_all()
            .expect("runtime audit events")
            .into_iter()
            .filter_map(|event| match event.kind {
                AuditKind::Note { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            runtime_notes
                .iter()
                .any(|note| note.starts_with("lab_runtime:coder_granted:"))
        );
        assert!(
            runtime_notes
                .iter()
                .any(|note| note.starts_with("lab_runtime:coder_released:"))
        );

        let mut reviewer = request(Role::Reviewer, project.id.as_str(), "smoke-reviewer");
        reviewer.prompt =
            "Review the unchanged worktree. Reply with exactly REVIEW: APPROVED. Do not use tools."
                .to_string();
        reviewer.timeout = Duration::from_secs(120);
        reviewer.max_turns = 1;
        reviewer.max_budget_usd = 0.25;
        let reviewer_run = match runtime
            .launch_reviewer(&reviewer, "SMOKE-1", None)
            .expect("launch reviewer")
        {
            ReviewerLaunch::Finished(run) => run,
            ReviewerLaunch::Wait(reason) => panic!("reviewer unexpectedly waited: {reason:?}"),
        };
        assert!(
            reviewer_run.spawn_error.is_none(),
            "{}",
            reviewer_run.spawn_error.unwrap_or_default()
        );
        let reviewer_output = reviewer_run.output.expect("reviewer output");
        assert_eq!(
            reviewer_output.exit_code,
            Some(0),
            "{}",
            reviewer_output.stderr_text()
        );
        assert!(
            runtime
                .scheduler()
                .review_queue()
                .expect("review queue")
                .is_empty()
        );
        assert!(
            runtime
                .scheduler()
                .running_review()
                .expect("running review")
                .is_none()
        );

        fs::remove_dir_all(sandbox_state.join("projects")).expect("remove project fixture");
        sandbox.destroy().expect("destroy jg-sandbox world");
    }

    #[test]
    fn mode_a_four_teams_wait_for_the_fifth_without_launching_it() {
        let (_temp, runtime) = fixture(Mode::A, FakeLauncher::default());
        for name in ["alpha", "beta", "gamma", "delta"] {
            assert!(matches!(
                runtime
                    .scheduler()
                    .acquire_coder(project(name), "backend")
                    .expect("acquire"),
                CoderGrant::Granted(_)
            ));
        }

        let result = runtime
            .launch_coder(&request(Role::Coder, "epsilon", "coder-5"), "backend", None)
            .expect("wait");
        assert!(matches!(result, CoderLaunch::Wait(WaitReason::TeamLimit)));
        assert_eq!(runtime.launcher.calls(), 0);
    }

    #[test]
    fn mode_b_allows_one_project_and_refuses_mixing_until_slots_are_released() {
        let (_temp, runtime) = fixture(Mode::B, FakeLauncher::default());
        for repo in ["api", "web", "e2e", "docs"] {
            assert!(matches!(
                runtime
                    .scheduler()
                    .acquire_coder(project("alpha"), repo)
                    .expect("acquire"),
                CoderGrant::Granted(_)
            ));
        }
        let result = runtime
            .launch_coder(&request(Role::Coder, "alpha", "coder-5"), "extra", None)
            .expect("wait");
        assert!(matches!(result, CoderLaunch::Wait(WaitReason::CoderLimit)));
        assert!(matches!(
            runtime
                .scheduler()
                .acquire_coder(project("beta"), "api")
                .expect("other project waits"),
            CoderGrant::Wait(WaitReason::OtherProjectRunning)
        ));
        assert!(matches!(
            runtime.scheduler().set_mode(Mode::A),
            Err(jgf_hub::SchedulerError::ModeLocked { .. })
        ));
        assert_eq!(runtime.launcher.calls(), 0);
    }

    #[test]
    fn coder_slots_release_after_success_failure_and_kill() {
        let failed = ChildRun {
            spawn_error: Some("spawn failed".to_string()),
            ..ChildRun::default()
        };
        let killed = ChildRun {
            killed: true,
            ..ChildRun::default()
        };
        let (temp, runtime) = fixture(
            Mode::A,
            FakeLauncher::with_runs([ChildRun::default(), failed, killed]),
        );

        for (project_name, worker) in [
            ("alpha", "coder-a"),
            ("beta", "coder-b"),
            ("gamma", "coder-c"),
        ] {
            assert!(matches!(
                runtime
                    .launch_coder(&request(Role::Coder, project_name, worker), "backend", None)
                    .expect("launch"),
                CoderLaunch::Finished(_)
            ));
            assert!(runtime.scheduler().coder_slots().expect("slots").is_empty());
        }

        let events = note_events(&temp);
        assert_eq!(
            events
                .iter()
                .filter(|kind| *kind == &AuditKind::note("lab_runtime:coder_released:1"))
                .count(),
            1
        );
        assert_eq!(
            events
                .iter()
                .filter(|kind| matches!(kind, AuditKind::Note { .. }))
                .count(),
            6
        );
    }

    #[test]
    fn queued_reviews_wait_for_the_running_review_and_then_start_fifo() {
        let (_temp, runtime) = fixture(Mode::A, FakeLauncher::default());
        runtime
            .scheduler()
            .request_review(project("alpha"), "ALPHA-1")
            .expect("queue first");
        assert_eq!(
            runtime.scheduler().start_review().expect("start first"),
            Some(ReviewTicket {
                project: project("alpha"),
                task: "ALPHA-1".to_string(),
            })
        );

        let second = request(Role::Reviewer, "beta", "reviewer-2");
        assert!(matches!(
            runtime
                .launch_reviewer(&second, "BETA-2", None)
                .expect("wait"),
            ReviewerLaunch::Wait(ReviewWaitReason::ReviewRunning)
        ));
        assert_eq!(runtime.launcher.calls(), 0);
        runtime.scheduler().finish_review().expect("finish first");

        assert!(matches!(
            runtime
                .launch_reviewer(&second, "BETA-2", None)
                .expect("start second"),
            ReviewerLaunch::Finished(_)
        ));
        assert!(
            runtime
                .scheduler()
                .running_review()
                .expect("running")
                .is_none()
        );
        assert_eq!(runtime.launcher.calls(), 1);
    }

    #[test]
    fn a_later_eligible_review_does_not_overtake_an_earlier_queue_entry() {
        let (_temp, runtime) = fixture(Mode::A, FakeLauncher::default());
        runtime
            .scheduler()
            .request_review(project("alpha"), "ALPHA-1")
            .expect("queue first");

        assert!(matches!(
            runtime
                .launch_reviewer(
                    &request(Role::Reviewer, "beta", "reviewer-b"),
                    "BETA-1",
                    None
                )
                .expect("wait behind first"),
            ReviewerLaunch::Wait(ReviewWaitReason::EarlierReview)
        ));
        assert_eq!(runtime.launcher.calls(), 0);

        assert!(matches!(
            runtime
                .launch_reviewer(
                    &request(Role::Reviewer, "alpha", "reviewer-a"),
                    "ALPHA-1",
                    None
                )
                .expect("start first"),
            ReviewerLaunch::Finished(_)
        ));
        assert!(matches!(
            runtime
                .launch_reviewer(
                    &request(Role::Reviewer, "beta", "reviewer-b"),
                    "BETA-1",
                    None
                )
                .expect("start second"),
            ReviewerLaunch::Finished(_)
        ));
        assert_eq!(runtime.launcher.calls(), 2);
    }

    #[test]
    fn review_waits_for_its_project_coder_and_retries_after_release() {
        let (_temp, runtime) = fixture(Mode::A, FakeLauncher::default());
        let slot = match runtime
            .scheduler()
            .acquire_coder(project("alpha"), "backend")
            .expect("acquire")
        {
            CoderGrant::Granted(slot) => slot,
            CoderGrant::Wait(reason) => panic!("unexpected wait: {reason:?}"),
        };
        let reviewer = request(Role::Reviewer, "alpha", "reviewer-1");
        assert!(matches!(
            runtime
                .launch_reviewer(&reviewer, "ALPHA-1", None)
                .expect("wait"),
            ReviewerLaunch::Wait(ReviewWaitReason::ProjectCoderRunning)
        ));
        assert_eq!(runtime.launcher.calls(), 0);

        runtime.scheduler().release(slot.id).expect("release coder");
        assert!(matches!(
            runtime
                .launch_reviewer(&reviewer, "ALPHA-1", None)
                .expect("start reviewer"),
            ReviewerLaunch::Finished(_)
        ));
        assert_eq!(runtime.launcher.calls(), 1);
    }

    #[test]
    fn coder_acquired_before_review_start_does_not_strand_another_review() {
        let launcher = FakeLauncher::default();
        launcher.acquire_coder_before_review(project("alpha"));
        let (_temp, runtime) = fixture(Mode::A, launcher);
        runtime
            .scheduler()
            .request_review(project("alpha"), "ALPHA-1")
            .expect("queue alpha");
        runtime
            .scheduler()
            .request_review(project("beta"), "BETA-1")
            .expect("queue beta");
        let alpha_reviewer = request(Role::Reviewer, "alpha", "reviewer-alpha");

        assert!(matches!(
            runtime
                .launch_reviewer_with_hook(&alpha_reviewer, "ALPHA-1", None, |scheduler| runtime
                    .launcher
                    .run_review_start_hook(scheduler),)
                .expect("alpha waits"),
            ReviewerLaunch::Wait(ReviewWaitReason::ProjectCoderRunning)
        ));
        assert!(
            runtime
                .scheduler()
                .running_review()
                .expect("running review")
                .is_none()
        );
        assert_eq!(
            runtime.scheduler().review_queue().expect("queue"),
            [
                ReviewTicket {
                    project: project("alpha"),
                    task: "ALPHA-1".to_string(),
                },
                ReviewTicket {
                    project: project("beta"),
                    task: "BETA-1".to_string(),
                },
            ]
        );
        assert_eq!(runtime.launcher.calls(), 0);

        assert!(matches!(
            runtime
                .launch_reviewer(
                    &request(Role::Reviewer, "beta", "reviewer-beta"),
                    "BETA-1",
                    None,
                )
                .expect("beta starts"),
            ReviewerLaunch::Finished(_)
        ));
        assert!(
            runtime
                .scheduler()
                .running_review()
                .expect("running review after beta")
                .is_none()
        );
        assert_eq!(runtime.launcher.calls(), 1);

        runtime
            .scheduler()
            .release(
                runtime
                    .launcher
                    .take_acquired_coder()
                    .expect("alpha coder slot")
                    .id,
            )
            .expect("release alpha coder");
        assert!(matches!(
            runtime
                .launch_reviewer(&alpha_reviewer, "ALPHA-1", None)
                .expect("alpha starts"),
            ReviewerLaunch::Finished(_)
        ));
        assert_eq!(runtime.launcher.calls(), 2);
        assert_eq!(
            note_events(&_temp),
            [
                AuditKind::note("lab_runtime:review_wait:project_coder_running"),
                AuditKind::note("lab_runtime:review_started"),
                AuditKind::note("lab_runtime:review_finished"),
                AuditKind::note("lab_runtime:review_started"),
                AuditKind::note("lab_runtime:review_finished"),
            ]
        );
    }

    #[test]
    fn pause_and_kill_grant_nothing_new() {
        let (_temp, runtime) = fixture(Mode::A, FakeLauncher::default());
        let active = match runtime
            .scheduler()
            .acquire_coder(project("running"), "backend")
            .expect("acquire active coder")
        {
            CoderGrant::Granted(slot) => slot,
            CoderGrant::Wait(reason) => panic!("unexpected wait: {reason:?}"),
        };
        runtime.scheduler().pause().expect("pause");
        assert!(matches!(
            runtime.launch_coder(&request(Role::Coder, "alpha", "coder"), "api", None),
            Err(LabRuntimeError::Paused)
        ));
        assert!(matches!(
            runtime.launch_reviewer(
                &request(Role::Reviewer, "alpha", "reviewer"),
                "ALPHA-1",
                None
            ),
            Err(LabRuntimeError::Paused)
        ));
        runtime.scheduler().resume().expect("resume");
        let stopped = runtime.kill().expect("kill");
        assert_eq!(stopped.slots.len(), 1);
        assert_eq!(stopped.slots[0].id, active.id);
        assert!(stopped.audit_error.is_none());
        assert!(matches!(
            runtime.launch_coder(&request(Role::Coder, "alpha", "coder"), "api", None),
            Err(LabRuntimeError::Killed)
        ));
        assert!(matches!(
            runtime.launch_reviewer(
                &request(Role::Reviewer, "alpha", "reviewer"),
                "ALPHA-1",
                None
            ),
            Err(LabRuntimeError::Killed)
        ));
        runtime
            .scheduler()
            .release(active.id)
            .expect("release stopped coder");
        assert_eq!(runtime.launcher.calls(), 0);
    }
}
