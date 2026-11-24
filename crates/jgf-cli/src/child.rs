//! Helpers for resolving and safely reporting real coder and reviewer children.

use std::{
    collections::BTreeSet,
    env,
    ffi::{OsStr, OsString},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const FAILURE_OUTPUT_TAIL_BYTES: usize = 2000;
const REDACTED: &str = "[redacted]";

/// Resolved Claude executable and the runtime directories needed to expose it read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuntimeLayout {
    pub executable: PathBuf,
    pub directories: Vec<PathBuf>,
    pub path: OsString,
}

/// Adds scrubbed, byte-bounded child output to a failure summary.
pub(crate) fn failure_reason(summary: &str, stdout: &str, stderr: &str) -> String {
    let mut reason = scrub_sensitive_text(summary);
    for (name, output) in [("stdout", stdout), ("stderr", stderr)] {
        let output = diagnostic_tail(output);
        if !output.is_empty() {
            reason.push_str(&format!("; {name}: {output}"));
        }
    }
    reason
}

/// Resolves Claude and its optional Node runtime without exposing task state or host secrets.
pub(crate) fn runtime_layout(
    binary: &Path,
    state_dir: &Path,
    work_tree: &Path,
    scratch: &Path,
    host_home: &Path,
) -> Result<RuntimeLayout, String> {
    let work_tree = canonical_dir(work_tree, "task work tree")?;
    validate_git_directory(&work_tree)?;
    let state_dir = canonical_dir(state_dir, "task state directory")?;
    let scratch = canonical_dir(scratch, "scratch home")?;
    let executable = resolve_executable(binary, &work_tree)?;
    let directories = runtime_dirs(&executable, &state_dir, &work_tree, &scratch, host_home)?;
    let path = child_path(&executable, &directories);
    Ok(RuntimeLayout {
        executable,
        directories,
        path,
    })
}

/// Refuses missing, non-directory, or symlink `.git` entries before mounting a work tree.
pub(crate) fn validate_git_directory(work_tree: &Path) -> Result<(), String> {
    let git_dir = work_tree.join(".git");
    let metadata = fs::symlink_metadata(&git_dir).map_err(|error| {
        format!(
            "could not inspect work-tree .git at {}: {error}",
            git_dir.display()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(format!(
            "work-tree .git must be a plain directory: {}",
            git_dir.display()
        ));
    }
    Ok(())
}

fn diagnostic_tail(text: &str) -> String {
    let mut start = text.len().saturating_sub(FAILURE_OUTPUT_TAIL_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    scrub_sensitive_text(&text[start..]).trim().to_string()
}

fn scrub_sensitive_text(text: &str) -> String {
    let mut safe_lines = String::new();
    for line in text.split_inclusive('\n') {
        let lowercase = line.to_ascii_lowercase();
        let credentials_output = [
            ".claude/.credentials.json",
            "claudeaioauth",
            "accesstoken",
            "refresh_token",
            "refreshtoken",
        ]
        .iter()
        .any(|marker| lowercase.contains(marker));
        if credentials_output {
            safe_lines.push_str("[credentials path output redacted]");
            if line.ends_with('\n') {
                safe_lines.push('\n');
            }
        } else {
            safe_lines.push_str(line);
        }
    }

    let bytes = safe_lines.as_bytes();
    let mut spans = Vec::new();

    for index in 0..bytes.len().saturating_sub(3) {
        if bytes[index] == b'g'
            && bytes[index + 1] == b'h'
            && matches!(bytes[index + 2], b'p' | b'o' | b'u' | b's' | b'r')
            && bytes[index + 3] == b'_'
        {
            let mut end = index + 4;
            while end < bytes.len() && bytes[end].is_ascii_alphanumeric() {
                end += 1;
            }
            if end - (index + 4) >= 20 {
                spans.push((index, end));
            }
        }
    }

    add_prefixed_token_spans(bytes, b"github_pat_", &mut spans);
    add_prefixed_token_spans(bytes, b"sk-ant-", &mut spans);
    add_bearer_spans(bytes, &mut spans);
    add_authorization_spans(bytes, &mut spans);

    replace_spans(&safe_lines, spans)
}

fn add_prefixed_token_spans(bytes: &[u8], prefix: &[u8], spans: &mut Vec<(usize, usize)>) {
    let mut offset = 0;
    while let Some(start) = find_ascii(bytes, prefix, offset, false) {
        let mut end = start + prefix.len();
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || matches!(bytes[end], b'_' | b'-'))
        {
            end += 1;
        }
        spans.push((start, end));
        offset = end.max(start + prefix.len());
    }
}

fn add_bearer_spans(bytes: &[u8], spans: &mut Vec<(usize, usize)>) {
    let prefix = b"Bearer ";
    let mut offset = 0;
    while let Some(start) = find_ascii(bytes, prefix, offset, true) {
        let mut end = start + prefix.len();
        while end < bytes.len()
            && !bytes[end].is_ascii_whitespace()
            && !matches!(bytes[end], b'"' | b'\'' | b',' | b'}' | b']')
        {
            end += 1;
        }
        spans.push((start, end));
        offset = end.max(start + prefix.len());
    }
}

fn add_authorization_spans(bytes: &[u8], spans: &mut Vec<(usize, usize)>) {
    let marker = b"Authorization";
    let mut offset = 0;
    while let Some(start) = find_ascii(bytes, marker, offset, true) {
        let end = bytes[start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |length| start + length);
        spans.push((start, end));
        offset = end.max(start + marker.len());
    }
}

fn find_ascii(bytes: &[u8], needle: &[u8], offset: usize, ignore_case: bool) -> Option<usize> {
    bytes
        .get(offset..)?
        .windows(needle.len())
        .position(|window| {
            if ignore_case {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            }
        })
        .map(|index| offset + index)
}

fn replace_spans(text: &str, mut spans: Vec<(usize, usize)>) -> String {
    spans.sort_unstable();
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in spans {
        if start < cursor {
            continue;
        }
        output.push_str(&text[cursor..start]);
        output.push_str(REDACTED);
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    output
}

fn canonical_dir(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = fs::canonicalize(path)
        .map_err(|error| format!("could not resolve {label} at {}: {error}", path.display()))?;
    if !path.is_dir() {
        return Err(format!("{label} is not a directory: {}", path.display()));
    }
    Ok(path)
}

fn resolve_executable(binary: &Path, work_tree: &Path) -> Result<PathBuf, String> {
    let candidate = if binary.is_absolute() {
        binary.to_path_buf()
    } else if binary.components().count() > 1 {
        work_tree.join(binary)
    } else {
        find_in_path(binary.as_os_str()).ok_or_else(|| {
            format!(
                "could not find Claude executable {:?} on PATH for jg-sandbox",
                binary
            )
        })?
    };
    let executable = fs::canonicalize(&candidate)
        .map_err(|error| format!("could not resolve Claude executable for jg-sandbox: {error}"))?;
    let metadata = fs::metadata(&executable)
        .map_err(|error| format!("could not inspect Claude executable for jg-sandbox: {error}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "Claude executable is not executable for jg-sandbox: {}",
            executable.display()
        ));
    }
    Ok(executable)
}

fn find_in_path(binary: &OsStr) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

fn runtime_dirs(
    executable: &Path,
    state_dir: &Path,
    work_tree: &Path,
    scratch: &Path,
    host_home: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut candidates = Vec::new();
    if let Some(parent) = executable.parent() {
        candidates.push(parent.to_path_buf());
    }

    if let Some(node) = find_in_path(OsStr::new("node")) {
        let node = fs::canonicalize(&node).map_err(|error| {
            format!("could not resolve the Node runtime for jg-sandbox: {error}")
        })?;
        if let Some(node_bin) = node.parent() {
            candidates.push(node_bin.to_path_buf());
            if let Some(node_root) = node_bin.parent() {
                candidates.push(node_root.to_path_buf());
            }
        }
    }

    let mut dirs = BTreeSet::new();
    for dir in candidates {
        if is_fixed_mount_path(&dir) || dir.starts_with(work_tree) || dir.starts_with(scratch) {
            continue;
        }
        if overlaps(&dir, state_dir) {
            return Err(format!(
                "refusing to expose task state through Claude runtime directory {}",
                dir.display()
            ));
        }
        if dir == host_home || host_home.starts_with(&dir) {
            return Err(format!(
                "refusing to expose the real home through Claude runtime directory {}",
                dir.display()
            ));
        }
        let forbidden = [
            host_home.join(".ssh"),
            host_home.join(".config").join("gh"),
            host_home.join(".git-credentials"),
            host_home.join(".claude").join(".credentials.json"),
        ];
        if forbidden.iter().any(|path| overlaps(&dir, path)) {
            return Err(format!(
                "refusing to expose a credential path through Claude runtime directory {}",
                dir.display()
            ));
        }
        dirs.insert(dir);
    }
    Ok(dirs.into_iter().collect())
}

fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn is_fixed_mount_path(path: &Path) -> bool {
    [
        "/usr", "/lib", "/lib64", "/lib32", "/libx32", "/bin", "/sbin", "/etc", "/proc", "/dev",
    ]
    .iter()
    .any(|root| path.starts_with(root))
}

fn child_path(executable: &Path, runtime_dirs: &[PathBuf]) -> OsString {
    let mut paths = vec![
        PathBuf::from("/usr/local/sbin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/sbin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/sbin"),
        PathBuf::from("/bin"),
    ];
    if let Some(parent) = executable.parent() {
        paths.push(parent.to_path_buf());
    }
    paths.extend(runtime_dirs.iter().filter_map(|dir| {
        if dir.file_name().is_some_and(|name| name == "bin") {
            Some(dir.clone())
        } else {
            None
        }
    }));
    let mut unique = BTreeSet::new();
    unique.extend(paths);
    env::join_paths(unique).unwrap_or_else(|_| OsString::from("/usr/bin:/bin"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_output_is_scrubbed_and_tailed_on_character_boundaries() {
        let fake_gh_token = format!("ghp_{}", "A".repeat(20));
        let output = format!(
            "before\n{fake_gh_token} github_pat_fake-placeholder sk-ant-fake-placeholder \
             Bearer fake-bearer-value\nAuthorization: fake-auth-value\n\
             /home/fake/.claude/.credentials.json: fake-credential-content\n\
             {{\"claudeAiOauth\":{{\"accessToken\":\"fake-oauth-value\"}}}}\n🦀"
        );
        let reason = failure_reason("coder failed", &output, "");

        assert!(reason.contains("🦀"));
        assert!(!reason.contains(&fake_gh_token));
        assert!(!reason.contains("github_pat_"));
        assert!(!reason.contains("sk-ant-"));
        assert!(!reason.contains("Bearer "));
        assert!(!reason.contains("Authorization"));
        assert!(!reason.contains("fake-auth-value"));
        assert!(!reason.contains(".credentials.json"));
        assert!(!reason.contains("fake-credential-content"));
        assert!(!reason.contains("fake-oauth-value"));
    }

    #[test]
    fn invalid_git_directory_is_rejected() {
        let root = tempfile::tempdir().expect("temp dir");
        let work_tree = root.path().join("work");
        fs::create_dir(&work_tree).expect("work tree");
        assert!(validate_git_directory(&work_tree).is_err());

        let git = work_tree.join(".git");
        fs::create_dir(&git).expect("git dir");
        assert!(validate_git_directory(&work_tree).is_ok());
    }
}
