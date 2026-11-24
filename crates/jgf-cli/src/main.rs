//! `jg-factory`: the Dark Factory command line.

use std::{
    path::PathBuf,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand};
use jgf_cli::e2e::{self, E2eConfig, RunOutcome};

/// Dark Factory: autonomous lab for building software from specs.
#[derive(Parser)]
#[command(name = "jg-factory", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the end-to-end toy scenario.
    E2e(E2eArgs),
}

#[derive(Args)]
struct E2eArgs {
    /// Ask a running end-to-end run to stop, or verify its audit log.
    #[command(subcommand)]
    action: Option<E2eAction>,

    /// GitHub owner the private repository is created under.
    #[arg(long)]
    owner: Option<String>,

    /// Repository name; defaults to a timestamped toy name.
    #[arg(long)]
    repo: Option<String>,

    /// Dark Factory data directory.
    #[arg(long = "state-dir")]
    state_dir: Option<PathBuf>,

    /// Run without network access, using the fake backend.
    #[arg(long)]
    dry_run: bool,

    /// Maximum number of pushes the runner may make in one day.
    #[arg(long, value_name = "N")]
    max_pushes_per_day: Option<u32>,

    /// Add a human co-author trailer instead of using the host's global Git identity.
    #[arg(long, conflicts_with = "no_co_author")]
    co_author: Option<String>,

    /// Do not add a human co-author trailer.
    #[arg(long, conflicts_with = "co_author")]
    no_co_author: bool,
}

#[derive(Subcommand)]
enum E2eAction {
    /// Create the kill file of the run using --state-dir.
    Kill(StateDirArgs),
    /// Verify the audit log of the run using --state-dir.
    Verify(StateDirArgs),
}

#[derive(Args)]
struct StateDirArgs {
    /// Dark Factory data directory.
    #[arg(long = "state-dir")]
    state_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::E2e(args)) => run_e2e(args),
        None => {
            println!(
                "jg-factory: state dir {}",
                jgf_core::paths::data_dir().display()
            );
            ExitCode::SUCCESS
        }
    }
}

fn run_e2e(args: E2eArgs) -> ExitCode {
    if let Some(action) = args.action {
        return match action {
            E2eAction::Kill(args) => request_kill(args),
            E2eAction::Verify(args) => verify(args),
        };
    }

    let owner = match args.owner {
        Some(owner) => owner,
        None if args.dry_run => "dry-run-owner".to_string(),
        None => {
            eprintln!("jg-factory e2e: --owner is required unless --dry-run is given");
            return ExitCode::FAILURE;
        }
    };
    let repo = args.repo.unwrap_or_else(default_repo_name);
    let state_dir = args.state_dir.unwrap_or_else(jgf_core::paths::data_dir);
    let fixture_dir = match e2e::default_fixture_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("jg-factory e2e: {error}");
            return ExitCode::FAILURE;
        }
    };

    let mut config = E2eConfig::new(owner, repo, state_dir, fixture_dir);
    config.dry_run = args.dry_run;
    if let Some(max_pushes_per_day) = args.max_pushes_per_day {
        config.max_pushes_per_day = max_pushes_per_day;
    }
    if let Some(co_author) = args.co_author.as_deref() {
        config.set_co_author_override(co_author);
    } else if args.no_co_author {
        config.disable_co_author();
    }

    let outcome = e2e::run(&config);
    println!("{outcome}");
    if outcome == RunOutcome::Delivered {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn request_kill(args: StateDirArgs) -> ExitCode {
    let state_dir = args.state_dir.unwrap_or_else(jgf_core::paths::data_dir);
    match e2e::request_kill(&state_dir) {
        Ok(path) => {
            println!("kill requested at {}", path.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("jg-factory e2e kill: {error}");
            ExitCode::FAILURE
        }
    }
}

fn verify(args: StateDirArgs) -> ExitCode {
    let state_dir = args.state_dir.unwrap_or_else(jgf_core::paths::data_dir);
    let report = e2e::verify_run(&state_dir);
    if report.ok() {
        let terminal = report.terminal.as_deref().unwrap_or("none");
        println!("verify: ok ({} events, outcome {terminal})", report.events);
        ExitCode::SUCCESS
    } else {
        for problem in &report.problems {
            eprintln!("verify: {problem}");
        }
        println!("verify: failed");
        ExitCode::FAILURE
    }
}

fn default_repo_name() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or_default();
    format!("jgf-e2e-toy-{millis}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn e2e_max_pushes_per_day_flag_parses_zero() {
        let cli = Cli::try_parse_from([
            "jg-factory",
            "e2e",
            "--dry-run",
            "--max-pushes-per-day",
            "0",
        ])
        .expect("parse e2e push cap");

        let Some(Command::E2e(args)) = cli.command else {
            panic!("expected e2e command");
        };
        assert_eq!(args.max_pushes_per_day, Some(0));
    }
}
