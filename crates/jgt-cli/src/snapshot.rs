//! `jg-team-dev snapshot`: what a verification checked, saved as a git tree under
//! `refs/jgt/<key>/checked-<round>` (PRD E4, Q10), so the next round looks only at the fix.
//!
//! The tree is written through a temporary index (`GIT_INDEX_FILE`): the branch, the real index
//! and the history stay untouched, and coders still never commit.

use crate::tasks::Board;
use anyhow::{Context, Result, bail};
use jgt_core::{Task, TaskStatus};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[derive(clap::Args)]
pub struct SnapshotArgs {
    /// The task (JGT-12).
    #[arg(long)]
    pub task: String,
    /// The verification round the snapshot records (default: the round the task just went through).
    #[arg(long)]
    pub round: Option<i64>,
    /// Print the diff of the worktree since the newest snapshot instead of saving one.
    #[arg(long)]
    pub diff: bool,
    /// With --diff: only the changed files and line counts.
    #[arg(long, requires = "diff")]
    pub stat: bool,
    /// The task's worktree (default: the one on the task, else `.team/worktrees/<key>`).
    #[arg(long)]
    pub worktree: Option<PathBuf>,
    /// The workspace (default: the Coordinator's folder, or the current one).
    #[arg(long)]
    pub workspace: Option<PathBuf>,
}

fn git(dir: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// The worktree as a tree object: tracked and untracked files, `.gitignore` respected.
pub(crate) fn tree(dir: &Path) -> Result<String> {
    let git_dir = PathBuf::from(git(dir, None, &["rev-parse", "--absolute-git-dir"])?);
    let index = git_dir.join(format!("jgt-snapshot-index-{}", std::process::id()));
    let result = (|| {
        // An empty repository has no HEAD: start from an empty index then.
        if git(dir, None, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
            git(dir, Some(&index), &["read-tree", "HEAD"])?;
        }
        git(dir, Some(&index), &["add", "-A"])?;
        git(dir, Some(&index), &["write-tree"])
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// `refs/jgt/jgt-12/checked-` for `JGT-12`.
fn prefix(key: &str) -> String {
    format!("refs/jgt/{}/checked-", key.trim().to_lowercase())
}

/// Saves the worktree as `refs/jgt/<key>/checked-<round>` and returns the ref.
pub(crate) fn save(dir: &Path, key: &str, round: i64) -> Result<String> {
    let name = format!("{}{round}", prefix(key));
    git(dir, None, &["update-ref", &name, &tree(dir)?])?;
    Ok(name)
}

/// The newest snapshot of the task: (round, ref).
pub(crate) fn latest(dir: &Path, key: &str) -> Result<Option<(i64, String)>> {
    let p = prefix(key);
    let refs = git(dir, None, &["for-each-ref", "--format=%(refname)", p.trim_end_matches("checked-")])?;
    Ok(refs
        .lines()
        .filter_map(|r| r.strip_prefix(&p).and_then(|n| n.parse::<i64>().ok()).map(|n| (n, r.to_string())))
        .max_by_key(|(n, _)| *n))
}

/// The diff of the worktree now against the newest snapshot, new files included.
pub(crate) fn diff(dir: &Path, key: &str, stat: bool) -> Result<String> {
    let Some((_, base)) = latest(dir, key)? else {
        bail!("{key} has no snapshot yet: run `jg-team-dev snapshot --task {key}` after a verification");
    };
    let now = tree(dir)?;
    let mut args = vec!["diff"];
    if stat {
        args.push("--stat");
    }
    args.extend([base.as_str(), now.as_str()]);
    git(dir, None, &args)
}

/// The round a snapshot taken now records: in Working the fix round already counted the
/// verification that sent it back; in Testing or Review it is the current one.
pub(crate) fn checked_round(task: &Task) -> i64 {
    let current = task.round().0;
    if task.status == TaskStatus::Working { (current - 1).max(1) } else { current }
}

/// The worktree of a task in `workspace`: the one on the card, else `.team/worktrees/<key>`.
fn task_worktree(workspace: &Path, task: Option<&Task>, key: &str) -> PathBuf {
    let name = task.and_then(|t| t.worktree.clone()).unwrap_or_else(|| key.trim().to_lowercase());
    workspace.join(".team").join("worktrees").join(name)
}

pub async fn run(args: SnapshotArgs) -> Result<()> {
    let board = Board::new(args.workspace.clone()).await?;
    let task: Option<Task> = board.client.get(&board.path(&args.task, "")).await.ok();
    let key = task.as_ref().map(|t| t.key.clone()).unwrap_or_else(|| args.task.trim().to_uppercase());
    if jgt_core::parse_task_key(&key).is_none() {
        bail!("`{}` is not a task key (JGT-12)", args.task);
    }
    let dir = args.worktree.clone().unwrap_or_else(|| task_worktree(Path::new(&board.workspace), task.as_ref(), &key));
    if !dir.is_dir() {
        bail!("{} does not exist; pass --worktree", dir.display());
    }
    if args.diff {
        let text = diff(&dir, &key, args.stat)?;
        println!("{}", if text.is_empty() { "No changes since the last snapshot." } else { &text });
        return Ok(());
    }
    let round = match (args.round, &task, latest(&dir, &key)?) {
        (Some(r), _, _) => r,
        (None, Some(t), _) => checked_round(t),
        (None, None, last) => last.map_or(1, |(n, _)| n + 1),
    };
    let name = save(&dir, &key, round)?;
    println!("Saved {name}. The next round reviews only `jg-team-dev snapshot --task {key} --diff`.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn snapshots_keep_the_branch_and_diff_only_the_fix() {
        let dir = std::env::temp_dir().join(format!("jgt-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q"]);
        std::fs::write(dir.join("a.rs"), "one\n").unwrap();
        sh(&dir, &["add", "a.rs"]);
        sh(&dir, &["commit", "-qm", "base"]);
        let head = git(&dir, None, &["rev-parse", "HEAD"]).unwrap();
        // The coder's first pass: a change and a new file, uncommitted.
        std::fs::write(dir.join("a.rs"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("new.rs"), "fn x() {}\n").unwrap();
        let status = git(&dir, None, &["status", "--short"]).unwrap();
        assert_eq!(save(&dir, "JGT-7", 1).unwrap(), "refs/jgt/jgt-7/checked-1");
        // Nothing else moved: the branch, the index and the worktree.
        assert_eq!(git(&dir, None, &["rev-parse", "HEAD"]).unwrap(), head);
        assert_eq!(git(&dir, None, &["status", "--short"]).unwrap(), status);
        // The fix touches one line and adds one file: the diff shows only that.
        std::fs::write(dir.join("new.rs"), "fn x() -> u8 { 1 }\n").unwrap();
        std::fs::write(dir.join("fix.rs"), "// fix\n").unwrap();
        let stat = diff(&dir, "jgt-7", true).unwrap();
        assert!(stat.contains("new.rs") && stat.contains("fix.rs") && !stat.contains("a.rs"), "{stat}");
        assert!(diff(&dir, "JGT-7", false).unwrap().contains("+fn x() -> u8 { 1 }"));
        save(&dir, "JGT-7", 2).unwrap();
        assert_eq!(latest(&dir, "JGT-7").unwrap().map(|(n, _)| n), Some(2));
        assert_eq!(diff(&dir, "JGT-7", true).unwrap(), "");
        assert!(diff(&dir, "JGT-8", false).is_err(), "no snapshot yet");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_checked_round_is_the_verification_just_done() {
        let mut t = Task::new("/w", 1, "x", TaskStatus::Review);
        assert_eq!(checked_round(&t), 1);
        // Sent back to Working: the fix round counted, the snapshot is of verification 1.
        t.status = TaskStatus::Working;
        t.review_rounds = 1;
        assert_eq!(checked_round(&t), 1);
        t.status = TaskStatus::Testing;
        assert_eq!(checked_round(&t), 2);
    }
}
