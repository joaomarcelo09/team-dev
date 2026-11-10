//! Async git and filesystem helpers for the IDE tab.
//!
//! Uses `tokio::process::Command` without `git2`, running read-only git commands
//! with `GIT_OPTIONAL_LOCKS=0`, `-C <root>`, and `--` before paths.

use crate::ide::{
    ChangeStatus, DiffContent, FileContent, FileList, MAX_DIFF_LINES, MAX_ENTRIES, MAX_FILE_BYTES,
    Source, SourceKind, diff_lines, is_binary, is_secret, parse_ls_files, parse_ls_tree,
    parse_name_status, parse_worktrees, safe_rel_path, should_skip_path,
};
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

/// Creates a new read-only `git` command for the specified root directory.
fn git_cmd(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root);
    cmd.env("GIT_OPTIONAL_LOCKS", "0");
    cmd.env("GIT_LITERAL_PATHSPECS", "1");
    cmd.stdin(std::process::Stdio::null());
    cmd
}

/// Resolves the default main reference (`main`, falling back to `origin/HEAD`).
pub async fn resolve_main_ref(root: &Path) -> Result<String, String> {
    for candidate in &["main", "origin/HEAD"] {
        let mut cmd = git_cmd(root);
        cmd.args(["rev-parse", "--verify", candidate]);
        if let Ok(out) = cmd.output().await
            && out.status.success()
        {
            return Ok((*candidate).to_string());
        }
    }
    Err("branch 'main' not found (and origin/HEAD not available)".to_string())
}

/// Resolves the merge-base between `base_ref` and `HEAD`.
async fn git_merge_base(root: &Path, base_ref: &str) -> Option<String> {
    let mut cmd = git_cmd(root);
    cmd.args(["merge-base", base_ref, "HEAD"]);
    let out = cmd.output().await.ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/// Lists all available sources: `Main` (committed `main` ref) prepended, followed by
/// worktrees under `<project>/.team/worktrees/`.
pub async fn list_sources(project: &Path) -> Result<Vec<Source>, String> {
    let mut sources = Vec::new();
    let main_source = Source {
        label: "Main".to_string(),
        root: project.to_path_buf(),
        kind: SourceKind::Main,
    };
    sources.push(main_source);

    let mut cmd = git_cmd(project);
    cmd.args(["worktree", "list", "--porcelain"]);
    let out = match cmd.output().await {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Ok(sources),
    };

    let wt_list = parse_worktrees(&out);
    let wt_dir = project.join(".team").join("worktrees");
    let canon_wt_dir = wt_dir.canonicalize().ok();

    let mut worktrees = Vec::new();
    for wt in wt_list {
        if wt.is_bare {
            continue;
        }
        let is_under_wt_dir = if let Some(ref c_dir) = canon_wt_dir {
            wt.path.canonicalize().map(|p| p.starts_with(c_dir)).unwrap_or(false)
        } else {
            wt.path.starts_with(&wt_dir)
        };

        if !is_under_wt_dir {
            continue;
        }

        // Stale/prunable worktrees: skip if folder missing on disk
        if !wt.path.is_dir() {
            continue;
        }

        let label = wt
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("worktree")
            .to_string();
        let branch = wt.branch.unwrap_or_else(|| "HEAD".to_string());

        worktrees.push(Source {
            label,
            root: wt.path,
            kind: SourceKind::Worktree { branch },
        });
    }

    worktrees.sort_by(|a, b| a.label.cmp(&b.label));
    sources.extend(worktrees);

    Ok(sources)
}

/// Lists all files for the given source (capped at 20,000, skipping ignored/internal directories).
pub async fn list_files(source: &Source) -> Result<FileList, String> {
    let mut files = Vec::new();

    match &source.kind {
        SourceKind::Main => {
            let main_ref = resolve_main_ref(&source.root).await?;
            let mut cmd = git_cmd(&source.root);
            cmd.args(["ls-tree", "-r", "-z", "--full-tree", &main_ref]);
            let out = cmd.output().await.map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            let entries = parse_ls_tree(&String::from_utf8_lossy(&out.stdout));
            for entry in entries {
                if !should_skip_path(&entry.path) {
                    files.push(entry.path);
                }
            }
        }
        SourceKind::Worktree { .. } => {
            let mut cmd = git_cmd(&source.root);
            cmd.args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"]);
            let out = cmd.output().await.map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            let mut paths = parse_ls_files(&String::from_utf8_lossy(&out.stdout));

            // Also include deleted files detected vs base commit so they appear in listing with D mark
            let main_ref = resolve_main_ref(&source.root).await.unwrap_or_else(|_| "main".to_string());
            let base = git_merge_base(&source.root, &main_ref).await.unwrap_or(main_ref);
            let mut diff_cmd = git_cmd(&source.root);
            diff_cmd.args(["diff", "--name-status", "-z", "--no-ext-diff", "--no-textconv", &base]);
            if let Ok(diff_out) = diff_cmd.output().await
                && diff_out.status.success()
            {
                let diff_entries = parse_name_status(&String::from_utf8_lossy(&diff_out.stdout));
                for de in diff_entries {
                    if de.status == ChangeStatus::Deleted {
                        paths.push(de.path);
                    }
                }
            }

            for path in paths {
                if !should_skip_path(&path) {
                    files.push(path);
                }
            }
        }
    }

    files.sort();
    files.dedup();
    let total_count = files.len();
    let truncated = total_count > MAX_ENTRIES;
    if truncated {
        files.truncate(MAX_ENTRIES);
    }
    Ok(FileList {
        files,
        truncated,
        total_count,
    })
}

/// Computes file changes for the given source:
/// Main has no changes; worktrees report changes vs `git merge-base main HEAD`.
pub async fn changes(source: &Source) -> Result<HashMap<String, ChangeStatus>, String> {
    let mut map = HashMap::new();

    match &source.kind {
        SourceKind::Main => Ok(map),
        SourceKind::Worktree { .. } => {
            let main_ref = resolve_main_ref(&source.root).await.unwrap_or_else(|_| "main".to_string());
            let base = git_merge_base(&source.root, &main_ref).await.unwrap_or(main_ref);

            let mut cmd = git_cmd(&source.root);
            cmd.args(["diff", "--name-status", "-z", "--no-ext-diff", "--no-textconv", &base]);
            let out = cmd.output().await.map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }
            let entries = parse_name_status(&String::from_utf8_lossy(&out.stdout));
            for entry in entries {
                if should_skip_path(&entry.path) {
                    continue;
                }
                if entry.status == ChangeStatus::Renamed {
                    map.insert(entry.path, ChangeStatus::Renamed);
                    if let Some(old) = entry.old_path
                        && !should_skip_path(&old)
                    {
                        map.insert(old, ChangeStatus::Deleted);
                    }
                } else {
                    map.insert(entry.path, entry.status);
                }
            }

            // Also include untracked files as Added (A)
            let mut untracked_cmd = git_cmd(&source.root);
            untracked_cmd.args(["ls-files", "-z", "--others", "--exclude-standard"]);
            if let Ok(untracked_out) = untracked_cmd.output().await
                && untracked_out.status.success()
            {
                let untracked = parse_ls_files(&String::from_utf8_lossy(&untracked_out.stdout));
                for u in untracked {
                    if !should_skip_path(&u) && !map.contains_key(&u) {
                        map.insert(u, ChangeStatus::Added);
                    }
                }
            }

            Ok(map)
        }
    }
}

/// Reads file contents for the given source and relative path, applying safety limits.
pub async fn read_file(source: &Source, path: &str) -> Result<FileContent, String> {
    let rel = safe_rel_path(path)?;
    let rel_str = rel.to_string_lossy();

    // Secrets refused BEFORE any read or git command
    if is_secret(&rel) {
        return Ok(FileContent::Notice("Secret file: not shown".to_string()));
    }

    match &source.kind {
        SourceKind::Main => {
            let main_ref = resolve_main_ref(&source.root).await?;

            // Symlink check via git ls-tree mode 120000
            let mut ls_cmd = git_cmd(&source.root);
            ls_cmd.args(["ls-tree", "-z", &main_ref, "--", &rel_str]);
            if let Ok(ls_out) = ls_cmd.output().await {
                let entries = parse_ls_tree(&String::from_utf8_lossy(&ls_out.stdout));
                if entries.iter().any(|e| e.is_symlink()) {
                    return Ok(FileContent::Notice("Symlink: not followed".to_string()));
                }
            }

            // Size check via git cat-file -s
            let mut cat_size = git_cmd(&source.root);
            cat_size.args(["cat-file", "-s", &format!("{main_ref}:{rel_str}")]);
            let size_out = cat_size.output().await.map_err(|e| e.to_string())?;
            if !size_out.status.success() {
                return Err(String::from_utf8_lossy(&size_out.stderr).trim().to_string());
            }
            let size_str = String::from_utf8_lossy(&size_out.stdout).trim().to_string();
            let size: u64 = size_str.parse().unwrap_or(0);
            if size > MAX_FILE_BYTES {
                return Ok(FileContent::Notice(format!(
                    "File too large to show ({:.1} MB)",
                    size as f64 / 1_048_576.0
                )));
            }

            // Content read via git cat-file blob
            let mut cat_blob = git_cmd(&source.root);
            cat_blob.args(["cat-file", "blob", &format!("{main_ref}:{rel_str}")]);
            let blob_out = cat_blob.output().await.map_err(|e| e.to_string())?;
            if !blob_out.status.success() {
                return Err(String::from_utf8_lossy(&blob_out.stderr).trim().to_string());
            }
            let bytes = blob_out.stdout;
            if is_binary(&bytes) {
                return Ok(FileContent::Notice("Binary file".to_string()));
            }
            match String::from_utf8(bytes) {
                Ok(text) => Ok(FileContent::Text(text)),
                Err(_) => Ok(FileContent::Notice("Binary file".to_string())),
            }
        }
        SourceKind::Worktree { .. } => {
            let full_path = source.root.join(&rel);

            let meta = match tokio::fs::symlink_metadata(&full_path).await {
                Ok(m) => m,
                Err(_) => {
                    return Ok(FileContent::Notice(
                        "File not found (may have been deleted)".to_string(),
                    ));
                }
            };

            // Symlink check
            if meta.file_type().is_symlink() {
                return Ok(FileContent::Notice("Symlink: not followed".to_string()));
            }

            // Refuse non-regular files (FIFOs, sockets, directories, etc.)
            if !meta.is_file() {
                return Ok(FileContent::Notice("Not a regular file".to_string()));
            }

            // Canonicalize check stays inside root
            let canon_root = source.root.canonicalize().map_err(|e| e.to_string())?;
            let canon_file = full_path.canonicalize().map_err(|e| e.to_string())?;
            if !canon_file.starts_with(&canon_root) {
                return Err("Path escapes root directory".to_string());
            }

            // File size cap 1 MB
            let size = meta.len();
            if size > MAX_FILE_BYTES {
                return Ok(FileContent::Notice(format!(
                    "File too large to show ({:.1} MB)",
                    size as f64 / 1_048_576.0
                )));
            }

            // Read content capped at MAX_FILE_BYTES + 1 bytes
            let file = tokio::fs::File::open(&full_path).await.map_err(|e| e.to_string())?;
            let mut handle = file.take(MAX_FILE_BYTES + 1);
            let mut bytes = Vec::new();
            handle.read_to_end(&mut bytes).await.map_err(|e| e.to_string())?;

            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Ok(FileContent::Notice(format!(
                    "File too large to show ({:.1} MB)",
                    bytes.len() as f64 / 1_048_576.0
                )));
            }

            if is_binary(&bytes) {
                return Ok(FileContent::Notice("Binary file".to_string()));
            }
            match String::from_utf8(bytes) {
                Ok(text) => Ok(FileContent::Text(text)),
                Err(_) => Ok(FileContent::Notice("Binary file".to_string())),
            }
        }
    }
}

/// Generates unified diff for the given file in the source.
pub async fn file_diff(source: &Source, path: &str) -> Result<DiffContent, String> {
    let rel = safe_rel_path(path)?;
    let rel_str = rel.to_string_lossy();

    // Secrets refused BEFORE any git command
    if is_secret(&rel) {
        return Ok(DiffContent::Notice("Secret file: not shown".to_string()));
    }

    match &source.kind {
        SourceKind::Main => Ok(DiffContent::Lines(Vec::new())),
        SourceKind::Worktree { .. } => {
            let full_path = source.root.join(&rel);

            // Symlink refusal
            if let Ok(meta) = tokio::fs::symlink_metadata(&full_path).await
                && meta.file_type().is_symlink()
            {
                return Ok(DiffContent::Notice("Symlink: not followed".to_string()));
            }

            // If file exists, ensure canonical path stays within root
            if full_path.exists() {
                let canon_root = source.root.canonicalize().map_err(|e| e.to_string())?;
                let canon_file = full_path.canonicalize().map_err(|e| e.to_string())?;
                if !canon_file.starts_with(&canon_root) {
                    return Err("Path escapes root directory".to_string());
                }
            }

            let main_ref = resolve_main_ref(&source.root).await.unwrap_or_else(|_| "main".to_string());
            let base = git_merge_base(&source.root, &main_ref).await.unwrap_or(main_ref);

            // Check if untracked:
            let mut match_cmd = git_cmd(&source.root);
            match_cmd.args(["ls-files", "--error-unmatch", "--", &rel_str]);
            let is_untracked = match match_cmd.output().await {
                Ok(out) => !out.status.success() && full_path.exists(),
                Err(_) => false,
            };

            let out = if is_untracked {
                let mut cmd = git_cmd(&source.root);
                cmd.args([
                    "diff",
                    "--no-index",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--",
                    "/dev/null",
                    &rel_str,
                ]);
                cmd.output().await.map_err(|e| e.to_string())?
            } else {
                let mut cmd = git_cmd(&source.root);
                cmd.args([
                    "diff",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    &base,
                    "--",
                    &rel_str,
                ]);
                cmd.output().await.map_err(|e| e.to_string())?
            };

            // Exit code 0 or 1 is normal for diff
            let code = out.status.code().unwrap_or(0);
            if code > 1 {
                return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
            }

            // Cap at 1 MB
            if out.stdout.len() as u64 > MAX_FILE_BYTES {
                return Ok(DiffContent::Notice(format!(
                    "Diff too large to show ({:.1} MB)",
                    out.stdout.len() as f64 / 1_048_576.0
                )));
            }

            let text = String::from_utf8_lossy(&out.stdout);
            if (text.contains("Binary files") && text.contains("differ")) || is_binary(&out.stdout) {
                return Ok(DiffContent::Notice("Binary file".to_string()));
            }

            let lines = diff_lines(&text);
            if lines.len() > MAX_DIFF_LINES {
                return Ok(DiffContent::Notice(
                    "Diff too large to show (exceeds 20,000 lines)".to_string(),
                ));
            }

            Ok(DiffContent::Lines(lines))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ide::DiffLine;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct TestRepo {
        dir: std::path::PathBuf,
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn run_sync(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .expect("git command failed");
        assert!(status.success());
    }

    fn setup_test_repo(name: &str) -> (TestRepo, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "jgt-ide-test-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        run_sync(&dir, &["init", "-q", "-b", "main"]);
        run_sync(&dir, &["config", "user.email", "test@example.com"]);
        run_sync(&dir, &["config", "user.name", "Tester"]);

        // Initial commit on main
        std::fs::write(dir.join("tracked.txt"), "line 1\nline 2\n").unwrap();
        std::fs::write(dir.join("to_delete.txt"), "delete me\n").unwrap();
        run_sync(&dir, &["add", "."]);
        run_sync(&dir, &["commit", "-q", "-m", "init"]);

        // Create worktree under .team/worktrees/jgt-wt1
        let wt_dir = dir.join(".team").join("worktrees").join("jgt-wt1");
        std::fs::create_dir_all(dir.join(".team").join("worktrees")).unwrap();
        run_sync(&dir, &["worktree", "add", "-b", "jgt/jgt-wt1", wt_dir.to_str().unwrap()]);

        (TestRepo { dir }, wt_dir)
    }

    #[tokio::test]
    async fn test_list_sources_finds_main_and_worktrees() {
        let (repo, wt_dir) = setup_test_repo("sources");
        let sources = list_sources(&repo.dir).await.unwrap();

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].label, "Main");
        assert_eq!(sources[0].kind, SourceKind::Main);
        assert_eq!(sources[0].root, repo.dir);

        assert_eq!(sources[1].label, "jgt-wt1");
        assert_eq!(
            sources[1].kind,
            SourceKind::Worktree {
                branch: "jgt/jgt-wt1".to_string()
            }
        );
        assert_eq!(sources[1].root, wt_dir);
    }

    #[tokio::test]
    async fn test_changes_detection_amd() {
        let (repo, wt_dir) = setup_test_repo("changes");
        let sources = list_sources(&repo.dir).await.unwrap();
        let wt_source = &sources[1];

        // Modified file
        std::fs::write(wt_dir.join("tracked.txt"), "line 1\nline 2 modified\n").unwrap();

        // Deleted file
        std::fs::remove_file(wt_dir.join("to_delete.txt")).unwrap();

        // Added / untracked file
        std::fs::write(wt_dir.join("new_file.txt"), "new file content\n").unwrap();

        let ch = changes(wt_source).await.unwrap();
        assert_eq!(ch.get("tracked.txt"), Some(&ChangeStatus::Modified));
        assert_eq!(ch.get("to_delete.txt"), Some(&ChangeStatus::Deleted));
        assert_eq!(ch.get("new_file.txt"), Some(&ChangeStatus::Added));
    }

    #[tokio::test]
    async fn test_read_file_and_diff_limits() {
        let (repo, wt_dir) = setup_test_repo("limits");
        let sources = list_sources(&repo.dir).await.unwrap();
        let wt_source = &sources[1];

        // 1. Normal file read
        let content = read_file(wt_source, "tracked.txt").await.unwrap();
        assert_eq!(content.text(), Some("line 1\nline 2\n"));

        // 2. Secret refused BEFORE any read or git command
        // Create fake secret in temporary test repo ONLY
        std::fs::write(wt_dir.join(".env"), "FAKE_KEY=fake_val").unwrap();
        let secret_res = read_file(wt_source, ".env").await.unwrap();
        assert_eq!(secret_res.notice(), Some("Secret file: not shown"));

        let diff_secret = file_diff(wt_source, ".env").await.unwrap();
        assert_eq!(diff_secret.notice(), Some("Secret file: not shown"));

        // 3. Binary file refused
        let mut bin_data = vec![b'x'; 200];
        bin_data[50] = 0;
        std::fs::write(wt_dir.join("bin.dat"), bin_data).unwrap();
        let bin_res = read_file(wt_source, "bin.dat").await.unwrap();
        assert_eq!(bin_res.notice(), Some("Binary file"));

        // 4. File size cap 1 MB
        let large_data = vec![b'A'; (MAX_FILE_BYTES + 1024) as usize];
        std::fs::write(wt_dir.join("large.txt"), large_data).unwrap();
        let large_res = read_file(wt_source, "large.txt").await.unwrap();
        assert!(
            large_res.notice().unwrap().contains("File too large to show"),
            "Expected size notice, got: {:?}",
            large_res
        );

        // 5. Symlink refused
        symlink(wt_dir.join("tracked.txt"), wt_dir.join("tracked_symlink")).unwrap();
        let sym_res = read_file(wt_source, "tracked_symlink").await.unwrap();
        assert_eq!(sym_res.notice(), Some("Symlink: not followed"));

        let sym_diff = file_diff(wt_source, "tracked_symlink").await.unwrap();
        assert_eq!(sym_diff.notice(), Some("Symlink: not followed"));

        // 6. Path safety rejection
        assert!(read_file(wt_source, "../secret.txt").await.is_err());
        assert!(read_file(wt_source, "/etc/passwd").await.is_err());
        assert!(file_diff(wt_source, "../outside").await.is_err());
    }

    #[tokio::test]
    async fn test_file_diff_outputs_lines() {
        let (repo, wt_dir) = setup_test_repo("diff");
        let sources = list_sources(&repo.dir).await.unwrap();
        let wt_source = &sources[1];

        // Modify tracked.txt
        std::fs::write(wt_dir.join("tracked.txt"), "line 1\nline 2 changed\nline 3 added\n").unwrap();

        let diff = file_diff(wt_source, "tracked.txt").await.unwrap();
        assert!(diff.is_lines());
        let lines = diff.lines().unwrap();
        assert!(lines.iter().any(|l| matches!(l, DiffLine::Del(s) if s.contains("line 2"))));
        assert!(lines.iter().any(|l| matches!(l, DiffLine::Add(s) if s.contains("line 2 changed"))));
        assert!(lines.iter().any(|l| matches!(l, DiffLine::Add(s) if s.contains("line 3 added"))));
    }

    #[tokio::test]
    async fn test_file_diff_untracked_option_injection() {
        let (repo, wt_dir) = setup_test_repo("option-inject");
        let sources = list_sources(&repo.dir).await.unwrap();
        let wt_source = &sources[1];

        // An untracked file named `--output=x.txt`
        let injected_name = "--output=x.txt";
        std::fs::write(wt_dir.join(injected_name), "malicious content\n").unwrap();

        // Target file x.txt should not exist or be created/modified
        let target_x = wt_dir.join("x.txt");
        assert!(!target_x.exists());

        let diff = file_diff(wt_source, injected_name).await.unwrap();
        assert!(diff.is_lines());
        let lines = diff.lines().unwrap();
        assert!(lines.iter().any(|l| matches!(l, DiffLine::Add(s) if s.contains("malicious content"))));

        // x.txt must NOT have been created by git diff option injection
        assert!(!target_x.exists(), "x.txt must not be created or truncated");
    }

    #[tokio::test]
    async fn test_file_diff_literal_pathspecs_wildcard_no_leak() {
        let (repo, wt_dir) = setup_test_repo("pathspec");
        let sources = list_sources(&repo.dir).await.unwrap();
        let wt_source = &sources[1];

        // Tracked and modified .env in the repo
        // Note: dummy fixture in temporary test directory only
        std::fs::write(wt_dir.join(".env"), "SECRET_TOKEN_12345=secret\n").unwrap();
        run_sync(&wt_dir, &["add", ".env"]);
        run_sync(&wt_dir, &["commit", "-q", "-m", "add env"]);
        std::fs::write(wt_dir.join(".env"), "SECRET_TOKEN_12345=modified_secret\n").unwrap();

        // Create file literally named "*"
        std::fs::write(wt_dir.join("*"), "literal star file content\n").unwrap();

        let diff = file_diff(wt_source, "*").await.unwrap();
        assert!(diff.is_lines());
        let lines = diff.lines().unwrap();

        // Diff must contain the star file content
        assert!(lines.iter().any(|l| matches!(l, DiffLine::Add(s) if s.contains("literal star file content"))));

        // Diff must NOT contain .env content!
        for line in lines {
            let s = match line {
                DiffLine::Add(s) | DiffLine::Del(s) | DiffLine::Hunk(s) | DiffLine::Meta(s) | DiffLine::Ctx(s) => s,
            };
            assert!(!s.contains("SECRET_TOKEN_12345"), "file_diff('*') must not contain .env content");
            assert!(!s.contains(".env"), "file_diff('*') must not leak .env filename or diff");
        }
    }

    #[tokio::test]
    async fn test_read_file_non_regular_file_refused() {
        let (repo, wt_dir) = setup_test_repo("nonregular");
        let sources = list_sources(&repo.dir).await.unwrap();
        let wt_source = &sources[1];

        // A directory passed as path
        std::fs::create_dir_all(wt_dir.join("subfolder")).unwrap();
        let res = read_file(wt_source, "subfolder").await.unwrap();
        assert_eq!(res.notice(), Some("Not a regular file"));
    }

    #[tokio::test]
    async fn test_list_files_returns_file_list() {
        let (repo, _wt_dir) = setup_test_repo("filelist");
        let sources = list_sources(&repo.dir).await.unwrap();
        let wt_source = &sources[1];

        let file_list = list_files(wt_source).await.unwrap();
        assert!(!file_list.truncated);
        assert_eq!(file_list.total_count, file_list.files.len());
        assert!(file_list.files.contains(&"tracked.txt".to_string()));
    }
}
