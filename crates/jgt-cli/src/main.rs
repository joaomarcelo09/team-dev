//! `jg-team-dev`: the command line side of jg-team-dev — MCP server and hooks for agents,
//! connectors for harnesses, and a few terminal helpers.

mod commands;
mod hooks;
mod mcp;
mod runs;
mod settings;
mod snapshot;
mod tasks;
mod team;
mod workers;
mod worktree;

use clap::{Parser, Subcommand};
use jgt_core::Harness;

#[derive(Parser)]
#[command(name = "jg-team-dev", version, about = "Talk with your AI agents from the jg-team-dev desktop app")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start (or wake) the Coordinator for a project folder (the current folder when omitted).
    Start { folder: Option<std::path::PathBuf> },
    /// Start a worker for the Coordinator running this command; returns at once and the result
    /// reaches the Coordinator as a message.
    Worker(workers::WorkerArgs),
    /// List the workers of the Coordinator running this command.
    Workers {
        /// Coordinator session id (default: the Coordinator running this command).
        #[arg(long)]
        parent: Option<String>,
    },
    /// Run records under .team/runs/: open one per request, close it with a final report.
    #[command(subcommand)]
    Run(runs::RunCommand),
    /// The workspace Board: list, create, move and inspect tasks (`JGT-<n>`).
    Task(tasks::TaskArgs),
    /// Save what a verification checked as `refs/jgt/<key>/checked-<round>`, or print the diff
    /// since then (`--diff`), so the next round looks only at the fix.
    Snapshot(snapshot::SnapshotArgs),
    /// Runs a worker and relays its result (started by `worker`).
    #[command(hide = true)]
    WorkerRun(workers::WorkerRunArgs),
    /// Hand the Coordinator's folder to a fresh Coordinator (run by the Coordinator after the user
    /// agreed); this one is retired.
    Reset {
        /// The task summary the fresh Coordinator should pick up from (under .team/tasks/).
        #[arg(long)]
        summary: Option<std::path::PathBuf>,
    },
    /// Print the Claude Code settings (hooks) that jg-team-dev sessions load with `--settings`.
    Settings {
        /// Print the MCP config for `--mcp-config` instead.
        #[arg(long)]
        mcp: bool,
    },
    /// Show whether the hub is running.
    Status,
    /// List sessions.
    Sessions {
        /// Include archived sessions.
        #[arg(long)]
        all: bool,
    },
    /// Show a session's messages.
    Messages {
        /// Session id, id prefix or title.
        session: String,
        #[arg(short = 'n', long, default_value_t = 20)]
        count: i64,
    },
    /// Send a message to a session as the user.
    Send {
        /// Session id, id prefix or title.
        session: String,
        text: Vec<String>,
    },
    /// Start a disconnected session again.
    Reconnect {
        /// Session id, id prefix or title.
        session: String,
        /// Open it in a terminal instead of the background.
        #[arg(long)]
        terminal: bool,
    },
    /// Open the desktop app (on a session, when given).
    Open { session: Option<String> },
    /// Talk to the user from your own scripts or agents.
    #[command(subcommand)]
    Agent(commands::AgentCommand),
    /// MCP server for agents (started by the harness).
    Mcp {
        #[arg(long, value_parser = parse_harness, default_value = "claude-code")]
        harness: Harness,
    },
    /// Claim the Coordinator role for this Claude Code session.
    ClaimCoordinator,
    /// Claude Code hook (started by Claude Code).
    Hook {
        /// Role of the session calling this hook (coordinator, worker, etc.).
        #[arg(long)]
        role: Option<String>,
        event: hooks::HookEvent,
    },
}

fn parse_harness(s: &str) -> Result<Harness, String> {
    s.parse()
}

#[tokio::main]
async fn main() {
    // Exit quietly when the output pipe closes (`jg-team-dev sessions | head`) instead of panicking.
    // SAFETY: only changes the process-wide disposition of SIGPIPE back to the default.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Mcp { harness } => mcp::run(harness).await,
        Command::ClaimCoordinator => team::claim_coordinator().await,
        Command::Hook { role, event } => hooks::run(event, role).await,
        Command::Start { folder } => team::start(folder).await,
        Command::Worker(args) => workers::start(args).await,
        Command::Workers { parent } => workers::list(parent).await,
        Command::WorkerRun(args) => workers::run(args).await,
        Command::Snapshot(args) => snapshot::run(args).await,
        Command::Run(cmd) => runs::command(cmd).await,
        Command::Task(args) => tasks::command(args).await,
        Command::Reset { summary } => team::reset(summary).await,
        Command::Settings { mcp } => settings::print(mcp),
        Command::Status => commands::status().await,
        Command::Sessions { all } => commands::sessions(all).await,
        Command::Messages { session, count } => commands::messages(&session, count).await,
        Command::Send { session, text } => commands::send(&session, &text.join(" ")).await,
        Command::Reconnect { session, terminal } => commands::reconnect(&session, terminal).await,
        Command::Open { session } => commands::open(session.as_deref()).await,
        Command::Agent(cmd) => commands::agent(cmd).await,
    };
    if let Err(e) = result {
        eprintln!("jg-team-dev: {e:#}");
        std::process::exit(1);
    }
}
