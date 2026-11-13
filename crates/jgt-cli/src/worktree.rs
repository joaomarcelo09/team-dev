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

/// The direct child folders of `dir` that are git repositories, sorted by name (hidden folders
/// and symlinks skipped).
pub fn child_repos(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut repos: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()) && !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.join(".git").exists())
        .collect();
    repos.sort();
    repos
}

/// `.team/worktrees/<name>` in the repository holding `dir`, created from HEAD on branch
/// `jgt/<name>` when it does not exist yet. Returns its path.
///
/// When `dir` is not in a repository but holds repositories (a multi-repo team folder), each
/// child repository `<repo>` gets its worktree at `dir/.team/worktrees/<name>/<repo>` and the
/// folder holding them is returned.
pub fn ensure(dir: &Path, name: &str) -> Result<PathBuf> {
    check_name(name)?;
    let Some(root) = repo_root(dir) else {
        let repos = child_repos(dir);
        if repos.is_empty() {
            bail!("--worktree needs a git repository (or a folder whose subfolders are git repositories)");
        }
        let base = dir.join(".team").join("worktrees").join(name);
        for repo in &repos {
            let repo_name = repo.file_name().context("repository folder name")?;
            ensure_at(repo, name, &base.join(repo_name))?;
        }
        return Ok(base);
    };
    ensure_at(&root, name, &root.join(".team").join("worktrees").join(name))
}

/// The worktree of the repository `root` at `path`, on branch `jgt/<name>`.
fn ensure_at(root: &Path, name: &str, path: &Path) -> Result<PathBuf> {
    exclude_team_dir(root)?;
    let path = path.to_path_buf();
    if path.join(".git").exists() {
        return Ok(path);
    }
    let branch = format!("{BRANCH_PREFIX}{name}");
    let path_str = path.to_string_lossy().into_owned();
    let branch_exists = git(root, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).is_ok();
    if branch_exists {
        git(root, &["worktree", "add", &path_str, &branch])?;
    } else {
        git(root, &["worktree", "add", "-b", &branch, &path_str, "HEAD"])?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(name: &str) -> PathBuf {
        init_repo(std::env::temp_dir().join(format!("jgt-wt-{name}-{}", std::process::id())))
    }

    fn init_repo(dir: PathBuf) -> PathBuf {
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

    #[test]
    fn a_folder_of_repositories_gets_one_worktree_per_repository() {
        let dir = std::env::temp_dir().join(format!("jgt-wt-multi-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("notes")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let api = init_repo(dir.join("api"));
        let web = init_repo(dir.join("web"));
        let wt = ensure(&dir, "multi").unwrap();
        assert_eq!(wt, dir.join(".team/worktrees/multi"));
        for (repo, name) in [(&api, "api"), (&web, "web")] {
            let sub = wt.join(name);
            assert_eq!(std::fs::read_to_string(sub.join("a.txt")).unwrap(), "hello\n");
            assert_eq!(git(&sub, &["branch", "--show-current"]).unwrap(), "jgt/multi");
            assert_eq!(git(repo, &["branch", "--show-current"]).unwrap(), "main");
            assert_eq!(git(repo, &["status", "--porcelain"]).unwrap(), "", "{name}: the live checkout stays clean");
        }
        assert!(!wt.join("notes").exists(), "only repositories get worktrees");
        // A second call (say, for the reviewer) reuses them.
        assert_eq!(ensure(&dir, "multi").unwrap(), wt);
        assert_eq!(git(&api, &["worktree", "list", "--porcelain"]).unwrap().matches("worktree ").count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
