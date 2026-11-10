//! Pure data layer for the IDE tab (no iced view code).

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_FILE_BYTES: u64 = 1_048_576; // 1 MB
pub const MAX_DIFF_LINES: usize = 20_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Main,
    Worktree { branch: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub label: String,
    pub root: PathBuf,
    pub kind: SourceKind,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

/// Finds the worktree source matching a task key (e.g. "JGT-34" matches folder "jgt-34").
///
/// Only matches worktree sources (not `Main`). Returns `None` if no matching worktree
/// exists or if `task_key` is empty.
pub fn source_for_task<'a>(sources: &'a [Source], task_key: &str) -> Option<&'a Source> {
    let target = task_key.trim().to_lowercase();
    if target.is_empty() {
        return None;
    }
    sources.iter().find(|s| {
        matches!(s.kind, SourceKind::Worktree { .. }) && s.label.to_lowercase() == target
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileList {
    pub files: Vec<String>,
    pub truncated: bool,
    pub total_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
}

impl ChangeStatus {
    pub fn badge(&self) -> &'static str {
        match self {
            Self::Modified => "M",
            Self::Added => "A",
            Self::Deleted => "D",
            Self::Renamed => "R",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Add(String),
    Del(String),
    Hunk(String),
    Meta(String),
    Ctx(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    Text(String),
    Notice(String),
}

impl FileContent {
    #[cfg(test)]
    pub fn notice(&self) -> Option<&str> {
        match self {
            Self::Notice(s) => Some(s),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffContent {
    Lines(Vec<DiffLine>),
    Notice(String),
}

impl DiffContent {
    #[cfg(test)]
    pub fn is_lines(&self) -> bool {
        matches!(self, Self::Lines(_))
    }

    #[cfg(test)]
    pub fn notice(&self) -> Option<&str> {
        match self {
            Self::Notice(s) => Some(s),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn lines(&self) -> Option<&[DiffLine]> {
        match self {
            Self::Lines(l) => Some(l),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub status: Option<ChangeStatus>,
}

impl FileEntry {
    pub fn new(path: impl Into<String>, status: Option<ChangeStatus>) -> Self {
        Self {
            path: path.into(),
            status,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeNode {
    Folder {
        name: String,
        path: String,
        children: Vec<TreeNode>,
        has_changes: bool,
    },
    File {
        name: String,
        path: String,
        status: Option<ChangeStatus>,
        is_secret: bool,
    },
}

impl TreeNode {
    pub fn name(&self) -> &str {
        match self {
            Self::Folder { name, .. } => name,
            Self::File { name, .. } => name,
        }
    }

    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Folder { .. })
    }

    pub fn has_changes(&self) -> bool {
        match self {
            Self::Folder { has_changes, .. } => *has_changes,
            Self::File { status, .. } => status.is_some(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub path: String,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub is_open: bool,
    pub status: Option<ChangeStatus>,
    pub is_secret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsTreeEntry {
    pub mode: String,
    pub obj_type: String,
    pub object: String,
    pub path: String,
}

impl LsTreeEntry {
    pub fn is_symlink(&self) -> bool {
        self.mode == "120000"
    }
}

/// Parses output of `git ls-tree -r -z --full-tree <ref>`.
pub fn parse_ls_tree(output: &str) -> Vec<LsTreeEntry> {
    let mut entries = Vec::new();
    for item in output.split('\0') {
        if item.is_empty() {
            continue;
        }
        if let Some((meta, path)) = item.split_once('\t') {
            let mut parts = meta.split_whitespace();
            if let (Some(mode), Some(obj_type), Some(object)) =
                (parts.next(), parts.next(), parts.next())
            {
                entries.push(LsTreeEntry {
                    mode: mode.to_string(),
                    obj_type: obj_type.to_string(),
                    object: object.to_string(),
                    path: path.to_string(),
                });
            }
        }
    }
    entries
}

/// Parses output of `git ls-files -z ...`.
pub fn parse_ls_files(output: &str) -> Vec<String> {
    output
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameStatusEntry {
    pub status: ChangeStatus,
    pub path: String,
    pub old_path: Option<String>,
}

/// Parses output of `git diff --name-status -z <base>`, including renames and copies.
pub fn parse_name_status(output: &str) -> Vec<NameStatusEntry> {
    let mut tokens = output.split('\0').filter(|s| !s.is_empty());
    let mut entries = Vec::new();
    while let Some(status_str) = tokens.next() {
        let status_str = status_str.trim();
        if status_str.is_empty() {
            continue;
        }
        let first_char = status_str.chars().next().unwrap_or('\0');
        match first_char {
            'R' => {
                if let (Some(old_path), Some(new_path)) = (tokens.next(), tokens.next()) {
                    entries.push(NameStatusEntry {
                        status: ChangeStatus::Renamed,
                        path: new_path.to_string(),
                        old_path: Some(old_path.to_string()),
                    });
                }
            }
            'C' => {
                if let (Some(old_path), Some(new_path)) = (tokens.next(), tokens.next()) {
                    entries.push(NameStatusEntry {
                        status: ChangeStatus::Added,
                        path: new_path.to_string(),
                        old_path: Some(old_path.to_string()),
                    });
                }
            }
            'M' => {
                if let Some(path) = tokens.next() {
                    entries.push(NameStatusEntry {
                        status: ChangeStatus::Modified,
                        path: path.to_string(),
                        old_path: None,
                    });
                }
            }
            'A' => {
                if let Some(path) = tokens.next() {
                    entries.push(NameStatusEntry {
                        status: ChangeStatus::Added,
                        path: path.to_string(),
                        old_path: None,
                    });
                }
            }
            'D' => {
                if let Some(path) = tokens.next() {
                    entries.push(NameStatusEntry {
                        status: ChangeStatus::Deleted,
                        path: path.to_string(),
                        old_path: None,
                    });
                }
            }
            'T' => {
                if let Some(path) = tokens.next() {
                    entries.push(NameStatusEntry {
                        status: ChangeStatus::Modified,
                        path: path.to_string(),
                        old_path: None,
                    });
                }
            }
            _ => {
                let _ = tokens.next();
            }
        }
    }
    entries
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub is_bare: bool,
    pub is_detached: bool,
    pub is_prunable: bool,
}

/// Parses output of `git worktree list --porcelain`.
pub fn parse_worktrees(output: &str) -> Vec<WorktreeInfo> {
    let mut list = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    let mut current_head: Option<String> = None;
    let mut current_bare = false;
    let mut current_detached = false;
    let mut current_prunable = false;

    let flush = |path: &mut Option<PathBuf>,
                 branch: &mut Option<String>,
                 head: &mut Option<String>,
                 bare: &mut bool,
                 detached: &mut bool,
                 prunable: &mut bool,
                 list: &mut Vec<WorktreeInfo>| {
        if let Some(p) = path.take() {
            list.push(WorktreeInfo {
                path: p,
                branch: branch.take(),
                head: head.take(),
                is_bare: *bare,
                is_detached: *detached,
                is_prunable: *prunable,
            });
            *bare = false;
            *detached = false;
            *prunable = false;
        }
    };

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            flush(
                &mut current_path,
                &mut current_branch,
                &mut current_head,
                &mut current_bare,
                &mut current_detached,
                &mut current_prunable,
                &mut list,
            );
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            flush(
                &mut current_path,
                &mut current_branch,
                &mut current_head,
                &mut current_bare,
                &mut current_detached,
                &mut current_prunable,
                &mut list,
            );
            current_path = Some(PathBuf::from(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            let b = rest.trim();
            let cleaned = b.strip_prefix("refs/heads/").unwrap_or(b);
            current_branch = Some(cleaned.to_string());
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            current_head = Some(rest.trim().to_string());
        } else if line == "bare" {
            current_bare = true;
        } else if line == "detached" {
            current_detached = true;
        } else if line.starts_with("prunable") {
            current_prunable = true;
        }
    }
    flush(
        &mut current_path,
        &mut current_branch,
        &mut current_head,
        &mut current_bare,
        &mut current_detached,
        &mut current_prunable,
        &mut list,
    );
    list
}

/// Identifies whether a path matches secret patterns.
///
/// Refuses `.env`, `.env.*` (except `.env.example`, `.env.sample`, `.env.template`),
/// `*.pem`, `*.key`, `id_rsa*`, `credentials*`, `*.p12`, `.npmrc`, `.pypirc`.
pub fn is_secret(path: impl AsRef<Path>) -> bool {
    let p = path.as_ref();
    for comp in p.components() {
        let name = comp.as_os_str().to_string_lossy();
        if is_secret_name(&name) {
            return true;
        }
    }
    false
}

fn is_secret_name(name: &str) -> bool {
    if name == ".env" || name.starts_with(".env.") {
        if name == ".env.example" || name == ".env.sample" || name == ".env.template" {
            return false;
        }
        return true;
    }
    if name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".p12") {
        return true;
    }
    if name.starts_with("id_rsa") || name.starts_with("credentials") {
        return true;
    }
    if name == ".npmrc" || name == ".pypirc" {
        return true;
    }
    false
}

/// Validates relative path: rejects absolute paths, `..` segments, and NUL bytes.
pub fn safe_rel_path(path: &str) -> Result<PathBuf, String> {
    if path.contains('\0') {
        return Err("path contains NUL byte".to_string());
    }
    let p = Path::new(path);
    if p.is_absolute() || path.starts_with('/') || path.starts_with('\\') {
        return Err("absolute path not allowed".to_string());
    }
    let mut normalized = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => return Err("parent directory '..' not allowed".to_string()),
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute path not allowed".to_string())
            }
            Component::CurDir => continue,
            Component::Normal(c) => normalized.push(c),
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err("empty path".to_string());
    }
    Ok(normalized)
}

/// Checks if content contains a NUL byte in the first 8 KB.
pub fn is_binary(bytes: &[u8]) -> bool {
    let check_len = bytes.len().min(8192);
    bytes[..check_len].contains(&0)
}

/// Classifies git unified diff lines into Add, Del, Hunk, Meta, or Ctx.
pub fn diff_lines(diff_text: &str) -> Vec<DiffLine> {
    let mut lines = Vec::new();
    for line in diff_text.lines() {
        if line.starts_with("@@") {
            lines.push(DiffLine::Hunk(line.to_string()));
        } else if line.starts_with("+++ ") || line.starts_with("--- ") {
            lines.push(DiffLine::Meta(line.to_string()));
        } else if line.starts_with('+') {
            lines.push(DiffLine::Add(line.to_string()));
        } else if line.starts_with('-') {
            lines.push(DiffLine::Del(line.to_string()));
        } else if line.starts_with(' ') {
            lines.push(DiffLine::Ctx(line.to_string()));
        } else {
            lines.push(DiffLine::Meta(line.to_string()));
        }
    }
    lines
}

/// Checks whether a path should be skipped (e.g. .git/, target/, node_modules/, .team/).
pub fn should_skip_path(path: &str) -> bool {
    let p = path.trim_start_matches('/');
    if p.starts_with(".git/")
        || p == ".git"
        || p.starts_with("target/")
        || p == "target"
        || p.starts_with("node_modules/")
        || p == "node_modules"
        || p.starts_with(".team/")
        || p == ".team"
    {
        return true;
    }
    for comp in Path::new(p).components() {
        let s = comp.as_os_str().to_string_lossy();
        if s == ".git" || s == "node_modules" || s == ".team" {
            return true;
        }
    }
    false
}

/// Builds a folder/file tree from a flat list of entries, folders first and sorted alphabetically.
pub fn build_tree(entries: &[FileEntry]) -> Vec<TreeNode> {
    struct DirNode {
        dirs: BTreeMap<String, DirNode>,
        files: Vec<FileEntry>,
    }

    impl DirNode {
        fn new() -> Self {
            Self {
                dirs: BTreeMap::new(),
                files: Vec::new(),
            }
        }
    }

    let mut root = DirNode::new();

    for entry in entries {
        let parts: Vec<&str> = entry.path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let mut curr = &mut root;
        for &dir in &parts[..parts.len() - 1] {
            curr = curr.dirs.entry(dir.to_string()).or_insert_with(DirNode::new);
        }
        curr.files.push(entry.clone());
    }

    fn convert(dir: DirNode, current_path: &str) -> Vec<TreeNode> {
        let mut nodes = Vec::new();

        for (name, sub) in dir.dirs {
            let path = if current_path.is_empty() {
                name.clone()
            } else {
                format!("{current_path}/{name}")
            };
            let children = convert(sub, &path);
            let has_changes = children.iter().any(|c| c.has_changes());
            nodes.push(TreeNode::Folder {
                name,
                path,
                children,
                has_changes,
            });
        }

        for file in dir.files {
            let name = file
                .path
                .rsplit_once('/')
                .map(|(_, n)| n)
                .unwrap_or(&file.path)
                .to_string();
            let is_sec = is_secret(&file.path);
            nodes.push(TreeNode::File {
                name,
                path: file.path,
                status: file.status,
                is_secret: is_sec,
            });
        }

        nodes.sort_by(|a, b| match (a.is_dir(), b.is_dir()) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name().cmp(b.name()),
        });

        nodes
    }

    convert(root, "")
}

/// Flattens the visible tree rows given the set of currently open directories and changed_only filter.
pub fn visible_rows(
    nodes: &[TreeNode],
    open_dirs: &HashSet<String>,
    changed_only: bool,
) -> Vec<TreeRow> {
    fn collect(
        nodes: &[TreeNode],
        depth: usize,
        open_dirs: &HashSet<String>,
        changed_only: bool,
        out: &mut Vec<TreeRow>,
    ) {
        for node in nodes {
            match node {
                TreeNode::Folder {
                    name,
                    path,
                    children,
                    has_changes,
                } => {
                    if changed_only && !has_changes {
                        continue;
                    }
                    let is_open = open_dirs.contains(path);
                    out.push(TreeRow {
                        path: path.clone(),
                        name: name.clone(),
                        depth,
                        is_dir: true,
                        is_open,
                        status: None,
                        is_secret: false,
                    });
                    if is_open {
                        collect(children, depth + 1, open_dirs, changed_only, out);
                    }
                }
                TreeNode::File {
                    name,
                    path,
                    status,
                    is_secret,
                } => {
                    if changed_only && status.is_none() {
                        continue;
                    }
                    out.push(TreeRow {
                        path: path.clone(),
                        name: name.clone(),
                        depth,
                        is_dir: false,
                        is_open: false,
                        status: *status,
                        is_secret: *is_secret,
                    });
                }
            }
        }
    }

    let mut out = Vec::new();
    collect(nodes, 0, open_dirs, changed_only, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ls_tree() {
        let raw = "100644 blob abc123\tCargo.toml\x00120000 blob def456\tsymlink_file\x00100644 blob ghi789\tsrc/main.rs\0";
        let entries = parse_ls_tree(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].mode, "100644");
        assert_eq!(entries[0].path, "Cargo.toml");
        assert!(!entries[0].is_symlink());

        assert_eq!(entries[1].mode, "120000");
        assert_eq!(entries[1].path, "symlink_file");
        assert!(entries[1].is_symlink());

        assert_eq!(entries[2].path, "src/main.rs");

        let spaces = "100644 blob aaa111\t leading.txt\x00100644 blob bbb222\ttrailing.txt \0";
        let spaces_entries = parse_ls_tree(spaces);
        assert_eq!(spaces_entries.len(), 2);
        assert_eq!(spaces_entries[0].path, " leading.txt");
        assert_eq!(spaces_entries[1].path, "trailing.txt ");
    }

    #[test]
    fn test_parse_ls_files() {
        let raw = "Cargo.toml\0src/main.rs\0src/ide.rs\0";
        let files = parse_ls_files(raw);
        assert_eq!(files, vec!["Cargo.toml", "src/main.rs", "src/ide.rs"]);

        let raw_spaces = " leading.txt\0trailing.txt \0\0";
        let files_spaces = parse_ls_files(raw_spaces);
        assert_eq!(files_spaces, vec![" leading.txt", "trailing.txt "]);
    }

    #[test]
    fn test_should_skip_path() {
        assert!(should_skip_path(".git"));
        assert!(should_skip_path(".git/config"));
        assert!(should_skip_path("sub/.git/HEAD"));
        assert!(should_skip_path("target"));
        assert!(should_skip_path("target/debug/app"));
        assert!(should_skip_path("node_modules"));
        assert!(should_skip_path("node_modules/pkg/index.js"));
        assert!(should_skip_path(".team"));
        assert!(should_skip_path(".team/briefs/plan.md"));
        assert!(should_skip_path(".team/agents.json"));
        assert!(should_skip_path(".team/worktrees/jgt-34"));
        assert!(should_skip_path("sub/.team/file.txt"));

        assert!(!should_skip_path("src/main.rs"));
        assert!(!should_skip_path("crates/jgt-desktop/Cargo.toml"));
        assert!(!should_skip_path("README.md"));
    }

    #[test]
    fn test_parse_name_status_basic() {
        let raw = "M\0src/main.rs\0A\0src/ide.rs\0D\0old.txt\0";
        let entries = parse_name_status(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            NameStatusEntry {
                status: ChangeStatus::Modified,
                path: "src/main.rs".to_string(),
                old_path: None,
            }
        );
        assert_eq!(
            entries[1],
            NameStatusEntry {
                status: ChangeStatus::Added,
                path: "src/ide.rs".to_string(),
                old_path: None,
            }
        );
        assert_eq!(
            entries[2],
            NameStatusEntry {
                status: ChangeStatus::Deleted,
                path: "old.txt".to_string(),
                old_path: None,
            }
        );
    }

    #[test]
    fn test_parse_name_status_renames() {
        let raw = "R100\0old_name.rs\0new_name.rs\0M\0src/main.rs\0";
        let entries = parse_name_status(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            NameStatusEntry {
                status: ChangeStatus::Renamed,
                path: "new_name.rs".to_string(),
                old_path: Some("old_name.rs".to_string()),
            }
        );
        assert_eq!(
            entries[1],
            NameStatusEntry {
                status: ChangeStatus::Modified,
                path: "src/main.rs".to_string(),
                old_path: None,
            }
        );
    }

    #[test]
    fn test_parse_worktrees() {
        let raw = "\
worktree /home/user/project
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /home/user/project/.team/worktrees/jgt-34
HEAD 2222222222222222222222222222222222222222
branch refs/heads/jgt/jgt-34

worktree /home/user/project/.team/worktrees/detached
HEAD 3333333333333333333333333333333333333333
detached
";
        let wts = parse_worktrees(raw);
        assert_eq!(wts.len(), 3);
        assert_eq!(wts[0].path, PathBuf::from("/home/user/project"));
        assert_eq!(wts[0].branch.as_deref(), Some("main"));
        assert!(!wts[0].is_detached);

        assert_eq!(
            wts[1].path,
            PathBuf::from("/home/user/project/.team/worktrees/jgt-34")
        );
        assert_eq!(wts[1].branch.as_deref(), Some("jgt/jgt-34"));
        assert!(!wts[1].is_detached);

        assert_eq!(
            wts[2].path,
            PathBuf::from("/home/user/project/.team/worktrees/detached")
        );
        assert_eq!(wts[2].branch, None);
        assert!(wts[2].is_detached);
    }

    #[test]
    fn test_is_secret() {
        // Secrets: .env, .env.* except allowed examples
        assert!(is_secret(".env"));
        assert!(is_secret(".env.local"));
        assert!(is_secret(".env.production"));
        assert!(is_secret("subfolder/.env"));
        assert!(is_secret("subfolder/.env.test"));

        // Allowed env examples
        assert!(!is_secret(".env.example"));
        assert!(!is_secret(".env.sample"));
        assert!(!is_secret(".env.template"));
        assert!(!is_secret("config/.env.example"));

        // Keys and certs
        assert!(is_secret("server.key"));
        assert!(is_secret("cert.pem"));
        assert!(is_secret("id_rsa"));
        assert!(is_secret("id_rsa.pub"));
        assert!(is_secret("keys/id_rsa_team"));
        assert!(is_secret("identity.p12"));

        // Credentials and package managers
        assert!(is_secret("credentials.json"));
        assert!(is_secret("credentials"));
        assert!(is_secret(".npmrc"));
        assert!(is_secret(".pypirc"));

        // Non-secrets
        assert!(!is_secret("src/main.rs"));
        assert!(!is_secret("Cargo.toml"));
        assert!(!is_secret("README.md"));
        assert!(!is_secret("src/keyboard.rs"));
    }

    #[test]
    fn test_safe_rel_path() {
        // Valid paths
        assert_eq!(
            safe_rel_path("src/main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(
            safe_rel_path("./Cargo.toml").unwrap(),
            PathBuf::from("Cargo.toml")
        );
        assert_eq!(
            safe_rel_path("crates/jgt-desktop/src/ide.rs").unwrap(),
            PathBuf::from("crates/jgt-desktop/src/ide.rs")
        );

        // Parent directory .. rejected
        assert!(safe_rel_path("../outside.rs").is_err());
        assert!(safe_rel_path("src/../../outside.rs").is_err());
        assert!(safe_rel_path("..").is_err());

        // Absolute paths rejected
        assert!(safe_rel_path("/etc/passwd").is_err());
        assert!(safe_rel_path("/home/user/file").is_err());

        // NUL byte rejected
        assert!(safe_rel_path("src/\0main.rs").is_err());

        // Empty path rejected
        assert!(safe_rel_path("").is_err());
        assert!(safe_rel_path(".").is_err());
    }

    #[test]
    fn test_is_binary() {
        let text = b"pub fn main() { println!(\"Hello world\"); }\n";
        assert!(!is_binary(text));

        let mut bin = vec![b'a'; 100];
        bin[42] = 0;
        assert!(is_binary(&bin));

        // NUL after 8 KB should not trigger first-8KB check
        let mut large = vec![b'b'; 10_000];
        large[9000] = 0;
        assert!(!is_binary(&large));
    }

    #[test]
    fn test_diff_lines() {
        let diff = "\
diff --git a/file.txt b/file.txt
index 1111111..2222222 100644
--- a/file.txt
+++ b/file.txt
@@ -1,3 +1,4 @@
 common line
-deleted line
+added line 1
+added line 2
";
        let lines = diff_lines(diff);
        assert_eq!(lines[0], DiffLine::Meta("diff --git a/file.txt b/file.txt".into()));
        assert_eq!(lines[1], DiffLine::Meta("index 1111111..2222222 100644".into()));
        assert_eq!(lines[2], DiffLine::Meta("--- a/file.txt".into()));
        assert_eq!(lines[3], DiffLine::Meta("+++ b/file.txt".into()));
        assert_eq!(lines[4], DiffLine::Hunk("@@ -1,3 +1,4 @@".into()));
        assert_eq!(lines[5], DiffLine::Ctx(" common line".into()));
        assert_eq!(lines[6], DiffLine::Del("-deleted line".into()));
        assert_eq!(lines[7], DiffLine::Add("+added line 1".into()));
        assert_eq!(lines[8], DiffLine::Add("+added line 2".into()));
    }

    #[test]
    fn test_build_tree_and_visible_rows() {
        let entries = vec![
            FileEntry::new("README.md", None),
            FileEntry::new("crates/jgt-desktop/Cargo.toml", None),
            FileEntry::new("crates/jgt-desktop/src/main.rs", Some(ChangeStatus::Modified)),
            FileEntry::new("crates/jgt-cli/Cargo.toml", None),
            FileEntry::new("docs/plan.md", Some(ChangeStatus::Added)),
        ];

        let tree = build_tree(&entries);

        // Folders first at root: crates, docs, then README.md
        assert_eq!(tree.len(), 3);
        assert_eq!(tree[0].name(), "crates");
        assert!(tree[0].is_dir());
        assert!(tree[0].has_changes());

        assert_eq!(tree[1].name(), "docs");
        assert!(tree[1].is_dir());
        assert!(tree[1].has_changes());

        assert_eq!(tree[2].name(), "README.md");
        assert!(!tree[2].is_dir());
        assert!(!tree[2].has_changes());

        // Test visible rows when closed
        let open_dirs = HashSet::new();
        let rows_closed = visible_rows(&tree, &open_dirs, false);
        assert_eq!(rows_closed.len(), 3);
        assert_eq!(rows_closed[0].name, "crates");
        assert_eq!(rows_closed[0].depth, 0);
        assert!(!rows_closed[0].is_open);

        // Test visible rows when opening "crates" and "crates/jgt-desktop"
        let mut open_dirs = HashSet::new();
        open_dirs.insert("crates".to_string());
        open_dirs.insert("crates/jgt-desktop".to_string());
        open_dirs.insert("crates/jgt-desktop/src".to_string());

        let rows_open = visible_rows(&tree, &open_dirs, false);
        let names: Vec<&str> = rows_open.iter().map(|r| r.name.as_str()).collect();
        // crates -> jgt-cli (closed), jgt-desktop (open) -> src (open) -> main.rs, Cargo.toml -> docs -> README.md
        assert!(names.contains(&"crates"));
        assert!(names.contains(&"jgt-cli"));
        assert!(names.contains(&"jgt-desktop"));
        assert!(names.contains(&"main.rs"));

        // Test changed_only filter: unchanged crates/jgt-cli and README.md are filtered out
        let rows_filtered = visible_rows(&tree, &open_dirs, true);
        let filtered_names: Vec<&str> = rows_filtered.iter().map(|r| r.name.as_str()).collect();
        assert!(filtered_names.contains(&"crates"));
        assert!(filtered_names.contains(&"jgt-desktop"));
        assert!(filtered_names.contains(&"src"));
        assert!(filtered_names.contains(&"main.rs"));
        assert!(filtered_names.contains(&"docs"));
        assert!(!filtered_names.contains(&"README.md"));
        assert!(!filtered_names.contains(&"jgt-cli"));
        assert!(!filtered_names.contains(&"Cargo.toml"));
    }

    #[test]
    fn test_source_for_task() {
        let sources = vec![
            Source {
                label: "Main".to_string(),
                root: PathBuf::from("/repo"),
                kind: SourceKind::Main,
            },
            Source {
                label: "jgt-34".to_string(),
                root: PathBuf::from("/repo/.team/worktrees/jgt-34"),
                kind: SourceKind::Worktree {
                    branch: "jgt/jgt-34".to_string(),
                },
            },
            Source {
                label: "other-wt".to_string(),
                root: PathBuf::from("/repo/.team/worktrees/other-wt"),
                kind: SourceKind::Worktree {
                    branch: "feat/other".to_string(),
                },
            },
        ];

        // Matching key with uppercase (e.g. JGT-34 -> jgt-34)
        let found = source_for_task(&sources, "JGT-34");
        assert!(found.is_some());
        assert_eq!(found.unwrap().label, "jgt-34");

        // Matching key with lowercase
        let found_lower = source_for_task(&sources, "jgt-34");
        assert!(found_lower.is_some());
        assert_eq!(found_lower.unwrap().label, "jgt-34");

        // Whitespace trimmed
        let found_spaces = source_for_task(&sources, "  JGT-34  ");
        assert!(found_spaces.is_some());
        assert_eq!(found_spaces.unwrap().label, "jgt-34");

        // No match -> None
        assert!(source_for_task(&sources, "JGT-99").is_none());

        // Empty key -> None
        assert!(source_for_task(&sources, "").is_none());
        assert!(source_for_task(&sources, "   ").is_none());

        // Main is never returned even if key matches "Main"
        assert!(source_for_task(&sources, "Main").is_none());
        assert!(source_for_task(&sources, "main").is_none());
    }
}
