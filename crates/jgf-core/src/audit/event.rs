//! Audit event types, their plain constructors, redaction and hashing.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{error::ModelError, secrets::SecretKey};

/// Text appended to a field that was shortened.
const TRUNCATED: &str = " [truncated]";
/// Argument that replaces the tail of a too long command line.
const TRUNCATED_ARGV: &str = "[truncated argv]";
/// Text printed instead of a redacted value.
const REDACTED: &str = "[redacted]";

/// Maximum length of one command argument.
const MAX_ARG_LEN: usize = 1024;
/// Maximum number of recorded command arguments.
const MAX_ARGS: usize = 128;
/// Maximum length of a free-form text field, such as a reason.
const MAX_TEXT_LEN: usize = 512;
/// Maximum length of a filesystem path.
const MAX_PATH_LEN: usize = 1024;
/// Maximum length of a network host name.
const MAX_HOST_LEN: usize = 255;
/// Maximum length of a URL.
const MAX_URL_LEN: usize = 1024;
/// Maximum length of a worker name.
const MAX_WORKER_LEN: usize = 128;
/// Maximum length of a GitHub owner or repository name.
const MAX_GITHUB_NAME_LEN: usize = 255;
/// Maximum length of a Git branch name.
const MAX_BRANCH_LEN: usize = 255;
/// Maximum length of a lab name.
pub(crate) const MAX_LAB_LEN: usize = 128;
/// Maximum length of a project id.
const MAX_PROJECT_LEN: usize = 64;
/// Maximum length of a check name.
const MAX_CHECK_NAME_LEN: usize = 256;

/// One event of a lab's audit log.
///
/// `seq` starts at 1 and grows by one per event. `prev_hash` is the `hash` of the event
/// before it, or the empty string for the first event. `hash` is SHA-256 over `seq`,
/// `at_ms`, `lab`, `project`, `worker`, the JSON of `kind` and `prev_hash`, all separated
/// by `|`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Position in the chain, starting at 1.
    pub seq: u64,
    /// Wall-clock time of the event, in milliseconds since the Unix epoch.
    pub at_ms: u64,
    /// Lab the event belongs to.
    pub lab: String,
    /// Project the event belongs to, when it is project-specific.
    pub project: Option<String>,
    /// Worker that caused the event, when known.
    pub worker: Option<String>,
    /// What happened.
    pub kind: AuditKind,
    /// Hash of the event before this one, or the empty string for the first event.
    pub prev_hash: String,
    /// SHA-256 of this event, including `prev_hash`.
    pub hash: String,
}

impl AuditEvent {
    /// Builds an event, caps its context fields and computes its hash.
    pub(crate) fn new(
        seq: u64,
        at_ms: u64,
        lab: impl Into<String>,
        project: Option<String>,
        worker: Option<String>,
        kind: AuditKind,
        prev_hash: impl Into<String>,
    ) -> Result<Self, ModelError> {
        let mut event = Self {
            seq,
            at_ms,
            lab: truncate(&lab.into(), MAX_LAB_LEN),
            project: project.map(|value| truncate(&value, MAX_PROJECT_LEN)),
            worker: worker.map(|value| truncate(&value, MAX_WORKER_LEN)),
            kind: kind.capped(),
            prev_hash: prev_hash.into(),
            hash: String::new(),
        };
        event.hash = event.compute_hash()?;
        Ok(event)
    }

    /// The SHA-256 hash of this event's chain fields.
    ///
    /// The event's own `hash` is not part of the input, so a stored event can be checked by
    /// recomputing it and comparing the result with the stored `hash`.
    pub fn compute_hash(&self) -> Result<String, ModelError> {
        let kind = serde_json::to_string(&self.kind)?;
        let input = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.seq,
            self.at_ms,
            self.lab,
            self.project.as_deref().unwrap_or(""),
            self.worker.as_deref().unwrap_or(""),
            kind,
            self.prev_hash,
        );
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        Ok(format!("{:x}", hasher.finalize()))
    }
}

/// What an audit event records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditKind {
    /// A command finished running.
    Command {
        /// Arguments of the command, with secret values redacted.
        argv: Vec<String>,
        /// Exit code, when the command exited normally.
        exit_code: Option<i32>,
        /// Signal that ended the command, when it was killed.
        signal: Option<i32>,
        /// Whether the command was killed after a timeout.
        timed_out: bool,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
    },
    /// A plain note about the run.
    Note {
        /// Note text.
        text: String,
    },
    /// An egress connection was allowed or refused.
    Network {
        /// Destination host.
        host: String,
        /// Destination port.
        port: u16,
        /// Whether the connection was allowed.
        allowed: bool,
        /// Why the decision was made.
        reason: String,
    },
    /// A write outside the allowed paths was refused.
    DeniedWrite {
        /// Path the write targeted.
        path: String,
        /// Why the write was refused.
        reason: String,
    },
    /// A GitHub action was allowed or refused by the credential broker.
    GithubAction {
        /// Action the caller asked for, such as `push_task_branch`.
        action: String,
        /// Owner (user or organization) the action targets.
        owner: String,
        /// Repository the action targets.
        repo: String,
        /// Branch the action targets, when it has one.
        branch: Option<String>,
        /// Whether the broker allowed the action.
        allowed: bool,
        /// Why the decision was made.
        reason: String,
    },
    /// A task card moved on the board.
    BoardMove {
        /// Task that moved.
        task: String,
        /// Column the task left.
        from: String,
        /// Column the task entered.
        to: String,
        /// Person or agent that moved it.
        who: String,
        /// Why it moved.
        reason: String,
        /// Checks that were required for the move.
        checks: Vec<CheckStatus>,
    },
    /// A CI run produced a result.
    CiResult {
        /// CI provider, such as `github`.
        provider: String,
        /// Run identifier inside the provider.
        run: String,
        /// Provider status, such as `success` or `failure`.
        status: String,
        /// URL of the run, when known.
        url: Option<String>,
    },
    /// A worker spent tokens or money.
    Cost {
        /// Worker that spent it.
        worker: String,
        /// Cost in millionths of a US dollar.
        usd_micros: u64,
        /// Input tokens consumed.
        tokens_in: u64,
        /// Output tokens produced.
        tokens_out: u64,
    },
}

impl AuditKind {
    /// A finished command. Arguments are capped and secret-looking values are redacted.
    pub fn command(
        argv: Vec<String>,
        exit_code: Option<i32>,
        signal: Option<i32>,
        timed_out: bool,
        duration_ms: u64,
    ) -> Self {
        Self::Command {
            argv,
            exit_code,
            signal,
            timed_out,
            duration_ms,
        }
        .capped()
    }

    /// A plain note.
    pub fn note(text: impl Into<String>) -> Self {
        Self::Note { text: text.into() }.capped()
    }

    /// An egress decision.
    pub fn network(
        host: impl Into<String>,
        port: u16,
        allowed: bool,
        reason: impl Into<String>,
    ) -> Self {
        Self::Network {
            host: host.into(),
            port,
            allowed,
            reason: reason.into(),
        }
        .capped()
    }

    /// A refused write.
    pub fn denied_write(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::DeniedWrite {
            path: path.into(),
            reason: reason.into(),
        }
        .capped()
    }

    /// A GitHub action decision made by the credential broker.
    pub fn github_action(
        action: impl Into<String>,
        owner: impl Into<String>,
        repo: impl Into<String>,
        branch: Option<String>,
        allowed: bool,
        reason: impl Into<String>,
    ) -> Self {
        Self::GithubAction {
            action: action.into(),
            owner: owner.into(),
            repo: repo.into(),
            branch,
            allowed,
            reason: reason.into(),
        }
        .capped()
    }

    /// A task card move.
    pub fn board_move(
        task: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
        who: impl Into<String>,
        reason: impl Into<String>,
        checks: Vec<CheckStatus>,
    ) -> Self {
        Self::BoardMove {
            task: task.into(),
            from: from.into(),
            to: to.into(),
            who: who.into(),
            reason: reason.into(),
            checks,
        }
        .capped()
    }

    /// A CI result.
    pub fn ci_result(
        provider: impl Into<String>,
        run: impl Into<String>,
        status: impl Into<String>,
        url: Option<String>,
    ) -> Self {
        Self::CiResult {
            provider: provider.into(),
            run: run.into(),
            status: status.into(),
            url,
        }
        .capped()
    }

    /// A worker cost entry.
    pub fn cost(
        worker: impl Into<String>,
        usd_micros: u64,
        tokens_in: u64,
        tokens_out: u64,
    ) -> Self {
        Self::Cost {
            worker: worker.into(),
            usd_micros,
            tokens_in,
            tokens_out,
        }
        .capped()
    }

    /// Short stable name of the variant, used as the JSON `kind` tag.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::Note { .. } => "note",
            Self::Network { .. } => "network",
            Self::DeniedWrite { .. } => "denied_write",
            Self::GithubAction { .. } => "github_action",
            Self::BoardMove { .. } => "board_move",
            Self::CiResult { .. } => "ci_result",
            Self::Cost { .. } => "cost",
        }
    }

    /// Caps every free-form field and redacts secret-looking command arguments.
    ///
    /// Applying it twice yields the same value.
    fn capped(self) -> Self {
        match self {
            Self::Command {
                argv,
                exit_code,
                signal,
                timed_out,
                duration_ms,
            } => Self::Command {
                argv: cap_argv(redact_argv(&argv, &[])),
                exit_code,
                signal,
                timed_out,
                duration_ms,
            },
            Self::Note { text } => Self::Note {
                text: truncate(&text, MAX_TEXT_LEN),
            },
            Self::Network {
                host,
                port,
                allowed,
                reason,
            } => Self::Network {
                host: truncate(&host, MAX_HOST_LEN),
                port,
                allowed,
                reason: truncate(&reason, MAX_TEXT_LEN),
            },
            Self::DeniedWrite { path, reason } => Self::DeniedWrite {
                path: truncate(&path, MAX_PATH_LEN),
                reason: truncate(&reason, MAX_TEXT_LEN),
            },
            Self::GithubAction {
                action,
                owner,
                repo,
                branch,
                allowed,
                reason,
            } => Self::GithubAction {
                action: truncate(&action, MAX_TEXT_LEN),
                owner: truncate(&owner, MAX_GITHUB_NAME_LEN),
                repo: truncate(&repo, MAX_GITHUB_NAME_LEN),
                branch: branch.map(|value| truncate(&value, MAX_BRANCH_LEN)),
                allowed,
                reason: truncate(&reason, MAX_TEXT_LEN),
            },
            Self::BoardMove {
                task,
                from,
                to,
                who,
                reason,
                checks,
            } => Self::BoardMove {
                task: truncate(&task, MAX_TEXT_LEN),
                from: truncate(&from, MAX_TEXT_LEN),
                to: truncate(&to, MAX_TEXT_LEN),
                who: truncate(&who, MAX_TEXT_LEN),
                reason: truncate(&reason, MAX_TEXT_LEN),
                checks: checks.into_iter().map(CheckStatus::capped).collect(),
            },
            Self::CiResult {
                provider,
                run,
                status,
                url,
            } => Self::CiResult {
                provider: truncate(&provider, MAX_TEXT_LEN),
                run: truncate(&run, MAX_TEXT_LEN),
                status: truncate(&status, MAX_TEXT_LEN),
                url: url.map(|value| truncate(&value, MAX_URL_LEN)),
            },
            Self::Cost {
                worker,
                usd_micros,
                tokens_in,
                tokens_out,
            } => Self::Cost {
                worker: truncate(&worker, MAX_WORKER_LEN),
                usd_micros,
                tokens_in,
                tokens_out,
            },
        }
    }
}

/// Result of one check that guarded a board move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckStatus {
    /// Name of the check.
    pub name: String,
    /// Outcome of the check.
    pub outcome: CheckOutcome,
}

impl CheckStatus {
    /// A check named `name` with `outcome`.
    pub fn new(name: impl Into<String>, outcome: CheckOutcome) -> Self {
        Self {
            name: name.into(),
            outcome,
        }
        .capped()
    }

    fn capped(self) -> Self {
        Self {
            name: truncate(&self.name, MAX_CHECK_NAME_LEN),
            outcome: self.outcome,
        }
    }
}

/// Outcome of a check that guarded a board move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckOutcome {
    /// The check passed.
    Pass,
    /// The check ran and failed.
    Fail,
    /// The check was required but has no result.
    Missing,
}

/// Copies `argv`, replacing the value of `KEY=value` arguments whose key is in
/// `secret_keys` or looks like a secret environment name (`*_TOKEN`, `*_SECRET`,
/// `*_PASSWORD`, `*_PASS`, `*_KEY`, `*_CREDENTIALS`).
///
/// Callers pass the project's secret keys before building an [`AuditKind::Command`].
pub fn redact_argv(argv: &[String], secret_keys: &[SecretKey]) -> Vec<String> {
    argv.iter()
        .map(|argument| redact_argument(argument, secret_keys))
        .collect()
}

fn redact_argument(argument: &str, secret_keys: &[SecretKey]) -> String {
    let Some((key, _value)) = argument.split_once('=') else {
        return argument.to_string();
    };
    if is_secret_name(key, secret_keys) {
        format!("{key}={REDACTED}")
    } else {
        argument.to_string()
    }
}

fn is_secret_name(key: &str, secret_keys: &[SecretKey]) -> bool {
    secret_keys.iter().any(|known| known.as_str() == key) || looks_like_secret_name(key)
}

fn looks_like_secret_name(key: &str) -> bool {
    if SecretKey::parse(key).is_err() {
        return false;
    }
    const SUFFIXES: [&str; 6] = [
        "_TOKEN",
        "_SECRET",
        "_PASSWORD",
        "_PASS",
        "_KEY",
        "_CREDENTIALS",
    ];
    const NAMES: [&str; 4] = ["TOKEN", "SECRET", "PASSWORD", "CREDENTIALS"];
    SUFFIXES.iter().any(|suffix| key.ends_with(suffix)) || NAMES.contains(&key)
}

fn cap_argv(argv: Vec<String>) -> Vec<String> {
    let mut argv: Vec<String> = argv
        .iter()
        .map(|argument| truncate(argument, MAX_ARG_LEN))
        .collect();
    if argv.len() > MAX_ARGS {
        argv.truncate(MAX_ARGS);
        argv.push(TRUNCATED_ARGV.to_string());
    }
    argv
}

/// Shortens `value` to at most `max_chars` characters, marking a truncation.
fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().nth(max_chars).is_none() {
        return value.to_string();
    }
    let keep = max_chars.saturating_sub(TRUNCATED.chars().count());
    let prefix: String = value.chars().take(keep).collect();
    format!("{prefix}{TRUNCATED}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(raw: &str) -> SecretKey {
        SecretKey::parse(raw).expect("valid secret key")
    }

    fn command_event() -> AuditEvent {
        AuditEvent::new(
            1,
            1_700_000_000_000,
            "alpha",
            Some("demo".into()),
            Some("worker-1".into()),
            AuditKind::command(
                vec!["cargo".into(), "test".into()],
                Some(0),
                None,
                false,
                42,
            ),
            "",
        )
        .expect("valid event")
    }

    #[test]
    fn constructors_produce_the_matching_tag() {
        let cases = [
            (
                AuditKind::command(Vec::new(), Some(0), None, false, 1),
                "command",
            ),
            (AuditKind::note("push cap: 30 per day"), "note"),
            (
                AuditKind::network("example.com", 443, true, "allowed"),
                "network",
            ),
            (AuditKind::denied_write("/tmp/x", "outside"), "denied_write"),
            (
                AuditKind::github_action(
                    "push_task_branch",
                    "acme",
                    "app",
                    Some("jgt/JGT-1".into()),
                    true,
                    "allowed",
                ),
                "github_action",
            ),
            (
                AuditKind::board_move("JGT-1", "todo", "doing", "dev", "start", Vec::new()),
                "board_move",
            ),
            (
                AuditKind::ci_result("github", "42", "success", None),
                "ci_result",
            ),
            (AuditKind::cost("worker-1", 10, 2, 3), "cost"),
        ];
        for (kind, tag) in cases {
            assert_eq!(kind.tag(), tag);
        }
    }

    #[test]
    fn command_redacts_known_and_secret_looking_values() {
        let argv = vec![
            "DATABASE_URL=postgres://user:pass@host/db".to_string(),
            "GITHUB_TOKEN=not-a-real-token".to_string(),
            "PATH=/usr/local/bin".to_string(),
            "plain".to_string(),
            "EMPTY=".to_string(),
        ];
        let redacted = redact_argv(&argv, &[key("DATABASE_URL")]);

        assert_eq!(redacted[0], "DATABASE_URL=[redacted]");
        assert_eq!(redacted[1], "GITHUB_TOKEN=[redacted]");
        assert_eq!(redacted[2], "PATH=/usr/local/bin");
        assert_eq!(redacted[3], "plain");
        assert_eq!(redacted[4], "EMPTY=");
    }

    #[test]
    fn command_constructor_redacts_secret_looking_arguments() {
        let kind = AuditKind::command(
            vec!["API_KEY=not-a-real-key".into(), "PATH=/bin".into()],
            Some(0),
            None,
            false,
            1,
        );
        let AuditKind::Command { argv, .. } = kind else {
            panic!("expected a command");
        };
        assert_eq!(argv[0], "API_KEY=[redacted]");
        assert_eq!(argv[1], "PATH=/bin");
    }

    #[test]
    fn long_fields_are_truncated_and_marked() {
        let long_argument = "a".repeat(MAX_ARG_LEN + 100);
        let kind = AuditKind::command(vec![long_argument], Some(0), None, false, 1);
        let AuditKind::Command { argv, .. } = kind else {
            panic!("expected a command");
        };
        assert!(argv[0].ends_with(TRUNCATED));
        assert!(argv[0].chars().count() <= MAX_ARG_LEN);

        let long_reason = "b".repeat(MAX_TEXT_LEN + 100);
        let kind = AuditKind::network("example.com", 443, false, long_reason);
        let AuditKind::Network { reason, .. } = kind else {
            panic!("expected a network event");
        };
        assert!(reason.ends_with(TRUNCATED));
        assert!(reason.chars().count() <= MAX_TEXT_LEN);
    }

    #[test]
    fn github_action_fields_are_capped_and_marked() {
        let long_name = "a".repeat(MAX_GITHUB_NAME_LEN + 50);
        let long_branch = "b".repeat(MAX_BRANCH_LEN + 50);
        let long_reason = "c".repeat(MAX_TEXT_LEN + 50);
        let kind = AuditKind::github_action(
            "push_task_branch",
            long_name.clone(),
            long_name,
            Some(long_branch),
            false,
            long_reason,
        );
        let AuditKind::GithubAction {
            owner,
            repo,
            branch,
            reason,
            allowed,
            ..
        } = kind
        else {
            panic!("expected a GitHub action event");
        };
        assert_eq!(owner.chars().count(), MAX_GITHUB_NAME_LEN);
        assert!(owner.ends_with(TRUNCATED));
        assert_eq!(repo.chars().count(), MAX_GITHUB_NAME_LEN);
        assert!(branch.expect("branch").ends_with(TRUNCATED));
        assert!(reason.ends_with(TRUNCATED));
        assert!(!allowed);
    }

    #[test]
    fn too_many_arguments_are_truncated_with_a_marker() {
        let argv: Vec<String> = (0..MAX_ARGS + 5)
            .map(|index| format!("arg{index}"))
            .collect();
        let kind = AuditKind::command(argv, Some(0), None, false, 1);
        let AuditKind::Command { argv, .. } = kind else {
            panic!("expected a command");
        };
        assert_eq!(argv.len(), MAX_ARGS + 1);
        assert_eq!(argv.last().expect("marker"), TRUNCATED_ARGV);
    }

    #[test]
    fn event_serializes_with_the_kind_tag() {
        let event = command_event();
        let value = serde_json::to_value(&event).expect("serialize");
        assert_eq!(value["kind"]["kind"], "command");
        assert_eq!(value["seq"], 1);
        assert_eq!(value["project"], "demo");
    }

    #[test]
    fn github_action_event_serializes_every_field() {
        let kind = AuditKind::github_action(
            "open_pr",
            "acme",
            "app",
            Some("jgt/JGT-97".into()),
            true,
            "allowed",
        );
        let event = AuditEvent::new(
            1,
            1_700_000_000_000,
            "alpha",
            None,
            Some("worker-1".into()),
            kind,
            "",
        )
        .expect("valid event");
        let value = serde_json::to_value(&event).expect("serialize");
        assert_eq!(value["kind"]["kind"], "github_action");
        assert_eq!(value["kind"]["action"], "open_pr");
        assert_eq!(value["kind"]["owner"], "acme");
        assert_eq!(value["kind"]["repo"], "app");
        assert_eq!(value["kind"]["branch"], "jgt/JGT-97");
        assert_eq!(value["kind"]["allowed"], true);
        assert_eq!(value["kind"]["reason"], "allowed");
    }

    #[test]
    fn hash_is_stable_and_covers_every_chained_field() {
        let event = command_event();
        let hash = event.compute_hash().expect("hash");
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, event.hash);
        assert_eq!(hash, event.compute_hash().expect("hash again"));

        let mut other = event.clone();
        other.seq += 1;
        assert_ne!(other.compute_hash().expect("hash"), hash);

        let mut other = event.clone();
        other.prev_hash = "0".repeat(64);
        assert_ne!(other.compute_hash().expect("hash"), hash);

        let mut other = event;
        other.kind = AuditKind::network("example.com", 443, true, "allowed");
        assert_ne!(other.compute_hash().expect("hash"), hash);
    }
}
