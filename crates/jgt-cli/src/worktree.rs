//! Git worktrees for coders: each coder works in `.team/worktrees/<name>` on branch `jgt/<name>`,
//! so nothing reaches the main worktree until the user agrees and the Coordinator merges it.

use anyhow::{Context, Result, bail};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

/// Branch prefix for worktree branches.
pub const BRANCH_PREFIX: &str = "jgt/";

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !out.status.success() {
        bail!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The top folder of the git repository holding `dir`, if any.
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"]).ok().map(PathBuf::from)
}

/// Keeps `.team/` out of `git status` with the repository's local exclude file (never committed).
pub fn exclude_team_dir(root: &Path) -> Result<()> {
    let common = PathBuf::from(git(root, &["rev-parse", "--path-format=absolute", "--git-common-dir"])?);
    let file = common.join("info").join("exclude");
    let current = std::fs::read_to_string(&file).unwrap_or_default();
    if current.lines().any(|l| l.trim() == ".team/") {
        return Ok(());
    }
    std::fs::create_dir_all(file.parent().unwrap_or(&common))?;
    let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
    std::fs::write(&file, format!("{current}{sep}# jg-team-dev: briefs, task summaries, worktrees\n.team/\n"))?;
    Ok(())
}

/// Worktree names become folder and branch names: keep them simple.
pub fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && !name.starts_with('-');
    if !ok {
        bail!("worktree name `{name}`: use lowercase letters, digits, - and _ (up to 64)");
    }
    Ok(())
}

/// `.team/worktrees/<name>` in the repository holding `dir`, created from HEAD on branch
/// `jgt/<name>` when it does not exist yet. Returns its path.
pub fn ensure(dir: &Path, name: &str) -> Result<PathBuf> {
    check_name(name)?;
    let root = repo_root(dir).context("--worktree needs a git repository")?;
    exclude_team_dir(&root)?;
    let path = root.join(".team").join("worktrees").join(name);
    if path.join(".git").exists() {
        return Ok(path);
    }
    let branch = format!("{BRANCH_PREFIX}{name}");
    let path_str = path.to_string_lossy().into_owned();
    let branch_exists = git(&root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if branch_exists {
        git(&root, &["worktree", "add", &path_str, &branch])?;
    } else {
        git(&root, &["worktree", "add", "-b", &branch, &path_str, "HEAD"])?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jgt-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            git(&dir, &args).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();
        git(&dir, &["add", "a.txt"]).unwrap();
        git(&dir, &["commit", "-q", "-m", "init"]).unwrap();
        dir
    }

    #[test]
    fn names_are_checked() {
        assert!(check_name("fix-navbar_2").is_ok());
        for bad in ["", "Fix", "a/b", "../x", "-x", "a b"] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn worktrees_are_created_once_and_excluded() {
        let dir = repo("create");
        let wt = ensure(&dir, "fix-a").unwrap();
        assert_eq!(wt, dir.canonicalize().unwrap().join(".team/worktrees/fix-a"));
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "hello\n");
        assert_eq!(git(&wt, &["branch", "--show-current"]).unwrap(), "jgt/fix-a");
        // A second call (say, for the reviewer) reuses it.
        assert_eq!(ensure(&dir, "fix-a").unwrap(), wt);
        // `.team/` never shows up in the main worktree's status.
        assert_eq!(git(&dir, &["status", "--porcelain"]).unwrap(), "");
        let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".team/").count(), 1);
        exclude_team_dir(&dir).unwrap();
        let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("\n.team/").count(), 1, "added only once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_branch_is_checked_out_again() {
        let dir = repo("branch");
        let wt = ensure(&dir, "again").unwrap();
        git(&dir, &["worktree", "remove", &wt.to_string_lossy()]).unwrap();
        assert_eq!(ensure(&dir, "again").unwrap(), wt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktrees_need_a_repository() {
        let dir = std::env::temp_dir().join(format!("jgt-wt-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = ensure(&dir, "x").unwrap_err().to_string();
        assert!(err.contains("needs a git repository"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
