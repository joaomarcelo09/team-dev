//! In-lab client entry point for the host-owned message shim.

use std::{fs, path::PathBuf};

use clap::{Args, Subcommand};
use jgf_core::ProjectId;
use jgf_hub::{HubClient, HubClientError, MessageKind, Role, WorkerKind};

/// In-lab mail commands.
#[derive(Args)]
pub struct MailArgs {
    #[command(subcommand)]
    pub action: MailAction,
}

/// An operation on the host-owned message router.
#[derive(Subcommand)]
pub enum MailAction {
    /// Read all unread messages.
    Inbox,
    /// Read the oldest unread message without acknowledging it.
    Next,
    /// Acknowledge all unread messages.
    Ack,
    /// Send a message through the host-enforced route table.
    Send(MailSendArgs),
}

/// Arguments for sending a message.
#[derive(Args)]
pub struct MailSendArgs {
    /// Recipient role: user, lab-coordinator, co-coordinator:<project> or worker:<project>:<kind>:<id>.
    #[arg(long)]
    to: String,
    /// Message kind, for example report or demand.
    #[arg(long)]
    kind: String,
    /// Message text, or @path to read the body from a file.
    #[arg(long)]
    body: String,
}

/// Creates a message client from `JG_HUB_SOCKET` and `JG_HUB_TOKEN`.
///
/// The token is consumed by the client protocol and should never be logged.
pub fn client_from_env() -> Result<HubClient, HubClientError> {
    HubClient::from_env()
}

/// Executes one in-lab mail command.
pub fn run(args: MailArgs) -> Result<(), String> {
    let client = client_from_env().map_err(|error| error.to_string())?;
    match args.action {
        MailAction::Inbox => {
            let messages = client.inbox().map_err(|error| error.to_string())?;
            print_json(serde_json::to_string_pretty(&messages))
        }
        MailAction::Next => {
            let message = client.next().map_err(|error| error.to_string())?;
            print_json(serde_json::to_string_pretty(&message))
        }
        MailAction::Ack => {
            let count = client.ack().map_err(|error| error.to_string())?;
            println!("{count}");
            Ok(())
        }
        MailAction::Send(args) => {
            let to = parse_role(&args.to)?;
            let kind = parse_kind(&args.kind)?;
            let body = read_body(&args.body)?;
            let project = role_project(&to);
            let message = client
                .send(to, kind, project, "", body)
                .map_err(|error| error.to_string())?;
            print_json(serde_json::to_string_pretty(&message))
        }
    }
}

fn read_body(value: &str) -> Result<String, String> {
    let Some(path) = value.strip_prefix('@') else {
        return Ok(value.to_string());
    };
    if path.is_empty() {
        return Err("message body file path after @ must not be empty".to_string());
    }
    fs::read_to_string(PathBuf::from(path))
        .map_err(|error| format!("could not read message body file {path:?}: {error}"))
}

fn parse_role(value: &str) -> Result<Role, String> {
    match value {
        "user" => Ok(Role::User),
        "lab-coordinator" | "lab_coordinator" => Ok(Role::LabCoordinator),
        _ => {
            if let Some(project) = value
                .strip_prefix("co-coordinator:")
                .or_else(|| value.strip_prefix("co_coordinator:"))
            {
                return ProjectId::parse(project)
                    .map(Role::CoCoordinator)
                    .map_err(|error| format!("invalid recipient role: {error}"));
            }
            if let Some(worker) = value.strip_prefix("worker:") {
                let mut parts = worker.splitn(3, ':');
                let project = parts.next().unwrap_or_default();
                let kind = parts.next().unwrap_or_default();
                let id = parts.next().unwrap_or_default();
                let project = ProjectId::parse(project)
                    .map_err(|error| format!("invalid recipient role: {error}"))?;
                let kind = parse_worker_kind(kind)?;
                return Role::worker(project, id, kind)
                    .map_err(|error| format!("invalid recipient role: {error}"));
            }
            Err(format!("unknown recipient role {value:?}"))
        }
    }
}

fn parse_worker_kind(value: &str) -> Result<WorkerKind, String> {
    match value {
        "coder" => Ok(WorkerKind::Coder),
        "tester" => Ok(WorkerKind::Tester),
        "reviewer" => Ok(WorkerKind::Reviewer),
        "git-coder" | "git_coder" => Ok(WorkerKind::GitCoder),
        _ => Err(format!("unknown worker kind {value:?}")),
    }
}

fn parse_kind(value: &str) -> Result<MessageKind, String> {
    match value.to_ascii_lowercase().replace('-', "_").as_str() {
        "user_request" => Ok(MessageKind::UserRequest),
        "report" => Ok(MessageKind::Report),
        "demand" => Ok(MessageKind::Demand),
        "worker_result" => Ok(MessageKind::WorkerResult),
        "instruction" => Ok(MessageKind::Instruction),
        "milestone" => Ok(MessageKind::Milestone),
        "failure" => Ok(MessageKind::Failure),
        "budget_event" => Ok(MessageKind::BudgetEvent),
        _ => Err(format!("unknown message kind {value:?}")),
    }
}

fn role_project(role: &Role) -> Option<ProjectId> {
    match role {
        Role::CoCoordinator(project) | Role::Worker { project, .. } => Some(project.clone()),
        Role::User | Role::LabCoordinator => None,
    }
}

fn print_json(output: Result<String, serde_json::Error>) -> Result<(), String> {
    let output = output.map_err(|error| format!("could not format message output: {error}"))?;
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_roles_and_kinds() {
        assert_eq!(parse_role("user").expect("user role"), Role::User);
        assert_eq!(
            parse_role("co-coordinator:project-a").expect("project role"),
            Role::CoCoordinator(ProjectId::parse("project-a").expect("project"))
        );
        assert_eq!(
            parse_kind("report").expect("report kind"),
            MessageKind::Report
        );
        assert_eq!(
            parse_kind("user-request").expect("request kind"),
            MessageKind::UserRequest
        );
    }
}
