//! The review step of an end-to-end run.
//!
//! A [`ReviewerRunner`] receives a read-only prompt (spec, task and the branch under review)
//! and answers [`ReviewDecision::Approved`] or [`ReviewDecision::Changes`]. The real runner
//! starts `claude -p` under jg-sandbox with the same scratch home as the coder.
//! [`FakeReviewerRunner`] is the dry-run and test double.
//!
//! The world exposes only the work tree (with `.git` read-only), the private scratch home, the
//! required read-only runtime paths and the Claude credentials file. Network egress is restricted
//! to Claude's API through the jg-sandbox proxy.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::{
    child,
    e2e::KillSwitch,
    sandbox::ClaudeSandbox,
    scratch::{ScratchError, ScratchHome},
};

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
    sandbox: Option<Arc<ClaudeSandbox>>,
}

impl ClaudeReviewerRunner {
    /// A runner that starts the installed `claude` command.
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from("claude"),
            sandbox: None,
        }
    }

    pub(crate) fn in_sandbox(sandbox: Arc<ClaudeSandbox>) -> Self {
        Self {
            binary: PathBuf::from("claude"),
            sandbox: Some(sandbox),
        }
    }

    fn scratch_home(&self, request: &ReviewRequest) -> Result<ScratchHome, ScratchError> {
        ScratchHome::prepare(&request.scratch_dir, &crate::scratch::real_home()?)
    }

    fn run_inner(&self, request: &ReviewRequest, kill: Option<&dyn KillSwitch>) -> ReviewRun {
        let argv = request.command();
        let started = Instant::now();

        let Some(sandbox) = &self.sandbox else {
            return ReviewRun {
                spawn_error: Some(
                    "jg-sandbox world is missing; refusing to run Claude unsandboxed".to_string(),
                ),
                duration_ms: elapsed_ms(started),
                ..ReviewRun::default()
            };
        };
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

        let child_run = sandbox.run_claude(
            &self.binary,
            &argv,
            &request.work_dir,
            &scratch,
            request.timeout,
            kill,
        );
        let Some(output) = child_run.output else {
            return ReviewRun {
                killed: child_run.killed,
                spawn_error: child_run.spawn_error,
                duration_ms: elapsed_ms(started),
                ..ReviewRun::default()
            };
        };
        let stdout = output.stdout_text();
        let stderr = output.stderr_text();
        let exit_code = output.exit_code;
        let signal = output.signal;
        let timed_out = output.timed_out;
        let mut spawn_error = child_run.spawn_error;
        if output.truncated {
            spawn_error = Some("jg-sandbox command output was truncated".to_string());
        }
        let failed =
            spawn_error.is_some() || (!timed_out && exit_code != Some(0) && signal.is_none());
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
            killed: child_run.killed,
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

fn mentions_budget(stdout: &str, stderr: &str) -> bool {
    let text = format!("{stdout}\n{stderr}").to_ascii_lowercase();
    text.contains("exceeded usd budget") || text.contains("budget exceeded")
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

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
    fn the_reviewer_fails_closed_without_a_jg_sandbox_world() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let runner = ClaudeReviewerRunner::new();

        let run = runner.run(&request(root.path()));

        assert!(!run.succeeded());
        assert_eq!(run.decision, ReviewDecision::Changes);
        assert!(
            run.failure_reason()
                .contains("refusing to run Claude unsandboxed")
        );
    }
}
