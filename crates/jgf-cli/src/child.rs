//! Helpers for the real coder and reviewer children.
//!
//! Claude runs under bubblewrap with a private PID namespace and a restricted filesystem view.
//! The task work tree is writable except for its read-only `.git` directory. Network access
//! remains open. The process group helper makes a kill or timeout stop the whole tree the child
//! started, not only the direct child.

use std::{
    collections::BTreeSet,
    env,
    ffi::{OsStr, OsString},
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
};

use crate::scratch::ScratchHome;

/// Bubblewrap is required for real Claude children; there is no host-mode fallback.
pub(crate) const BWRAP_BINARY: &str = "/usr/bin/bwrap";

const FAILURE_OUTPUT_TAIL_BYTES: usize = 2000;
const REDACTED: &str = "[redacted]";

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

/// Filesystem and environment configuration consumed by the pure argument builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BwrapSpec {
    pub work_tree: PathBuf,
    pub scratch: PathBuf,
    pub credentials_file: PathBuf,
    pub read_only_runtime_dirs: Vec<PathBuf>,
    pub env: Vec<(OsString, OsString)>,
    pub child_argv: Vec<OsString>,
}

/// Builds the complete bubblewrap argument vector without inspecting the host.
pub(crate) fn bwrap_args(spec: &BwrapSpec) -> Vec<OsString> {
    let mut args = vec![
        "--die-with-parent".into(),
        "--unshare-pid".into(),
        "--ro-bind".into(),
        "/usr".into(),
        "/usr".into(),
    ];
    for path in ["/lib", "/lib64", "/lib32", "/libx32"] {
        args.extend(["--ro-bind-try".into(), path.into(), path.into()]);
    }
    for path in ["/bin", "/sbin", "/etc"] {
        args.extend(["--ro-bind".into(), path.into(), path.into()]);
    }
    // `/etc/resolv.conf` is a symlink into this directory on systemd-resolved hosts; without it
    // the child cannot resolve names (EAI_AGAIN).
    args.extend([
        "--ro-bind-try".into(),
        "/run/systemd/resolve".into(),
        "/run/systemd/resolve".into(),
    ]);
    args.extend([
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
    ]);

    let mut parent_dirs = BTreeSet::new();
    add_parent_dirs(&mut parent_dirs, &spec.work_tree);
    add_parent_dirs(&mut parent_dirs, &spec.scratch);
    add_parent_dirs(&mut parent_dirs, &spec.credentials_file);
    for dir in &spec.read_only_runtime_dirs {
        if !is_fixed_mount_path(dir)
            && !dir.starts_with(&spec.work_tree)
            && !dir.starts_with(&spec.scratch)
        {
            parent_dirs.insert(dir.clone());
            add_parent_dirs(&mut parent_dirs, dir);
        }
    }
    let mut parent_dirs: Vec<_> = parent_dirs.into_iter().collect();
    parent_dirs.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    for dir in parent_dirs {
        args.extend(["--dir".into(), dir.into()]);
    }

    for dir in &spec.read_only_runtime_dirs {
        if !is_fixed_mount_path(dir)
            && !dir.starts_with(&spec.work_tree)
            && !dir.starts_with(&spec.scratch)
        {
            args.extend([
                "--ro-bind".into(),
                dir.as_os_str().into(),
                dir.as_os_str().into(),
            ]);
        }
    }
    args.extend([
        "--bind".into(),
        spec.work_tree.as_os_str().into(),
        spec.work_tree.as_os_str().into(),
        "--ro-bind".into(),
        spec.work_tree.join(".git").into_os_string(),
        spec.work_tree.join(".git").into_os_string(),
        "--bind".into(),
        spec.scratch.as_os_str().into(),
        spec.scratch.as_os_str().into(),
        "--ro-bind".into(),
        spec.credentials_file.as_os_str().into(),
        spec.credentials_file.as_os_str().into(),
        "--chdir".into(),
        spec.work_tree.as_os_str().into(),
        "--clearenv".into(),
    ]);
    for (key, value) in &spec.env {
        args.extend(["--setenv".into(), key.clone(), value.clone()]);
    }
    args.push("--".into());
    args.extend(spec.child_argv.iter().cloned());
    args
}

/// Creates a bubblewrap command for the Claude executable in `argv`.
pub(crate) fn sandbox_command(
    bwrap_binary: &Path,
    binary: &Path,
    argv: &[String],
    state_dir: &Path,
    work_tree: &Path,
    scratch: &ScratchHome,
) -> Result<Command, String> {
    let state_dir = canonical_dir(state_dir, "task state directory")?;
    let work_tree = canonical_dir(work_tree, "task work tree")?;
    validate_git_directory(&work_tree)?;
    let scratch_root = canonical_dir(scratch.root(), "scratch home")?;
    let executable = resolve_executable(binary, &work_tree)?;
    let runtime_dirs = runtime_dirs(
        &executable,
        &state_dir,
        &work_tree,
        &scratch_root,
        scratch.host_home(),
    )?;
    let path = child_path(&executable, &runtime_dirs);
    let mut env = scratch.environment();
    set_env(&mut env, "PATH", path);

    let mut child_argv = Vec::with_capacity(argv.len());
    child_argv.push(executable.into_os_string());
    child_argv.extend(argv.iter().skip(1).map(OsString::from));
    let spec = BwrapSpec {
        work_tree,
        scratch: scratch_root,
        credentials_file: scratch.credentials_file().to_path_buf(),
        read_only_runtime_dirs: runtime_dirs,
        env,
        child_argv,
    };

    let mut command = Command::new(bwrap_binary);
    command.args(bwrap_args(&spec)).env_clear();
    Ok(command)
}

fn validate_git_directory(work_tree: &Path) -> Result<(), String> {
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

/// Turns bubblewrap startup diagnostics, including namespace failures, into a clear error.
pub(crate) fn bwrap_start_error(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .find(|line| line.starts_with("bwrap:"))
        .map(|line| format!("bwrap could not start the Claude child: {}", line.trim()))
}

/// Puts `command` in a new process group led by the child itself.
pub(crate) fn own_process_group(command: &mut Command) {
    command.process_group(0);
}

/// Kills the process group of `child` and reaps the direct child.
pub(crate) fn kill_process_group(child: &mut Child) -> Option<ExitStatus> {
    let pid = child.id();
    if pid != 0 {
        // SAFETY: the child was started in its own group, so the negative pid names that
        // group and a reused pid cannot reach an unrelated group of this process.
        let killed = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        if killed != 0 {
            let _ = child.kill();
        }
    } else {
        let _ = child.kill();
    }
    child.wait().ok()
}

fn add_parent_dirs(dirs: &mut BTreeSet<PathBuf>, path: &Path) {
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == Path::new("/") {
            break;
        }
        if dir != Path::new("/tmp") && !is_base_mount_path(dir) {
            dirs.insert(dir.to_path_buf());
        }
        parent = dir.parent();
    }
}

fn is_base_mount_path(path: &Path) -> bool {
    path.starts_with("/tmp") || is_fixed_mount_path(path)
}

fn is_fixed_mount_path(path: &Path) -> bool {
    [
        "/usr", "/lib", "/lib64", "/lib32", "/libx32", "/bin", "/sbin", "/etc", "/proc", "/dev",
    ]
    .iter()
    .any(|root| path.starts_with(root))
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
                "could not find Claude executable {:?} on PATH for bwrap",
                binary
            )
        })?
    };
    let executable = fs::canonicalize(&candidate)
        .map_err(|error| format!("could not resolve Claude executable for bwrap: {error}"))?;
    let metadata = fs::metadata(&executable)
        .map_err(|error| format!("could not inspect Claude executable for bwrap: {error}"))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(format!(
            "Claude executable is not executable for bwrap: {}",
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
        let node = fs::canonicalize(&node)
            .map_err(|error| format!("could not resolve the Node runtime for bwrap: {error}"))?;
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

fn set_env(env: &mut Vec<(OsString, OsString)>, key: &str, value: OsString) {
    if let Some((_, old_value)) = env.iter_mut().find(|(old_key, _)| old_key == key) {
        *old_value = value;
    } else {
        env.push((key.into(), value));
    }
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
        let reason = failure_reason("coder exited with status 1", &output, "");

        assert!(reason.contains("coder exited with status 1"));
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
    fn failure_output_keeps_only_the_last_two_thousand_bytes() {
        let output = format!("{}{}", "x".repeat(2000), "🦀");
        let reason = failure_reason("failed", &output, "");
        let diagnostic = reason
            .strip_prefix("failed; stdout: ")
            .expect("stdout is included");

        assert!(diagnostic.len() <= 2000);
        assert!(diagnostic.ends_with("🦀"));
        assert!(diagnostic.starts_with('x'));
    }

    fn arg(spec: &BwrapSpec) -> Vec<String> {
        bwrap_args(spec)
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    fn test_spec(root: &Path) -> BwrapSpec {
        let home = root.join("real-home");
        let scratch = root.join("state/scratch/lab");
        BwrapSpec {
            work_tree: root.join("state/e2e/repo"),
            scratch: scratch.clone(),
            credentials_file: home.join(".claude/.credentials.json"),
            read_only_runtime_dirs: vec![PathBuf::from("/opt/claude-runtime")],
            env: vec![
                ("HOME".into(), scratch.join("home").into_os_string()),
                (
                    "XDG_CONFIG_HOME".into(),
                    scratch.join("config").into_os_string(),
                ),
                (
                    "XDG_DATA_HOME".into(),
                    scratch.join("data").into_os_string(),
                ),
                (
                    "XDG_CACHE_HOME".into(),
                    scratch.join("cache").into_os_string(),
                ),
                ("GH_CONFIG_DIR".into(), scratch.join("gh").into_os_string()),
                (
                    "CLAUDE_CONFIG_DIR".into(),
                    scratch.join("claude").into_os_string(),
                ),
                ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
                ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
                ("GIT_TERMINAL_PROMPT".into(), "0".into()),
                ("TMPDIR".into(), "/tmp".into()),
                ("PATH".into(), "/usr/bin:/bin:/opt/claude-runtime".into()),
            ],
            child_argv: vec!["/opt/claude-runtime/claude".into(), "-p".into()],
        }
    }

    #[test]
    fn the_bwrap_arguments_bind_only_allowed_paths_and_use_scratch_environment() {
        let root = Path::new("/tmp/bwrap-args-test");
        let spec = test_spec(root);
        let args = arg(&spec);

        assert!(args.contains(&"--die-with-parent".to_string()));
        assert!(args.contains(&"--unshare-pid".to_string()));
        assert!(!args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--clearenv".to_string()));
        assert!(
            args.windows(3)
                .any(|window| { window == ["--ro-bind", "/usr", "/usr"] })
        );

        let mut writable_binds = Vec::new();
        let mut read_only_binds = Vec::new();
        for pair in args.windows(3) {
            if pair[0] == "--bind" {
                writable_binds.push((pair[1].clone(), pair[2].clone()));
            }
            if pair[0] == "--ro-bind" || pair[0] == "--ro-bind-try" {
                read_only_binds.push((pair[1].clone(), pair[2].clone()));
            }
        }
        assert_eq!(
            writable_binds,
            vec![
                (
                    spec.work_tree.display().to_string(),
                    spec.work_tree.display().to_string()
                ),
                (
                    spec.scratch.display().to_string(),
                    spec.scratch.display().to_string()
                ),
            ]
        );
        let work_bind = args
            .windows(3)
            .position(|window| {
                window
                    == [
                        "--bind",
                        "/tmp/bwrap-args-test/state/e2e/repo",
                        "/tmp/bwrap-args-test/state/e2e/repo",
                    ]
            })
            .expect("writable work-tree bind");
        let git_read_only_bind = args
            .windows(3)
            .position(|window| {
                window
                    == [
                        "--ro-bind",
                        "/tmp/bwrap-args-test/state/e2e/repo/.git",
                        "/tmp/bwrap-args-test/state/e2e/repo/.git",
                    ]
            })
            .expect("read-only .git bind");
        assert!(git_read_only_bind > work_bind);
        assert!(read_only_binds.contains(&(
            spec.credentials_file.display().to_string(),
            spec.credentials_file.display().to_string()
        )));
        assert!(read_only_binds.contains(&(
            spec.work_tree.join(".git").display().to_string(),
            spec.work_tree.join(".git").display().to_string()
        )));

        let real_home = root.join("real-home");
        let state = root.join("state");
        let forbidden = [
            real_home.join(".ssh"),
            real_home.join(".config/gh"),
            real_home.join(".git-credentials"),
            state.join("secrets"),
            state.join("broker"),
        ];
        for forbidden in forbidden {
            let overlaps_forbidden = |(source, destination): &(String, String)| {
                [Path::new(source), Path::new(destination)]
                    .iter()
                    .any(|bound| bound.starts_with(&forbidden) || forbidden.starts_with(bound))
            };
            assert!(
                !read_only_binds.iter().any(overlaps_forbidden),
                "read-only bind overlapped forbidden path: {}",
                forbidden.display()
            );
            assert!(
                !writable_binds.iter().any(overlaps_forbidden),
                "writable bind overlapped forbidden path: {}",
                forbidden.display()
            );
        }
        for (source, destination) in &read_only_binds {
            for bound in [Path::new(source), Path::new(destination)] {
                assert!(
                    bound != real_home && !real_home.starts_with(bound),
                    "real home or an ancestor was bound: {}",
                    bound.display()
                );
                assert!(
                    bound == spec.credentials_file || !bound.starts_with(&real_home),
                    "more than the credentials file was bound from real home: {}",
                    bound.display()
                );
                assert!(
                    bound == spec.work_tree.join(".git")
                        || (!bound.starts_with(&state) && !state.starts_with(bound)),
                    "task state was read-only bound: {}",
                    bound.display()
                );
            }
        }

        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--setenv",
                    "HOME",
                    spec.scratch.join("home").to_str().unwrap(),
                ]
        }));
        for (key, path) in [
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("GH_CONFIG_DIR", "gh"),
        ] {
            assert!(args.windows(3).any(|window| {
                window == ["--setenv", key, spec.scratch.join(path).to_str().unwrap()]
            }));
        }
        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--setenv",
                    "XDG_CONFIG_HOME",
                    spec.scratch.join("config").to_str().unwrap(),
                ]
        }));
        assert!(args.windows(3).any(|window| {
            window
                == [
                    "--setenv",
                    "CLAUDE_CONFIG_DIR",
                    spec.scratch.join("claude").to_str().unwrap(),
                ]
        }));
        for (key, value) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("TMPDIR", "/tmp"),
        ] {
            assert!(
                args.windows(3)
                    .any(|window| window == ["--setenv", key, value])
            );
        }
        for key in ["GITHUB_TOKEN", "GH_TOKEN", "SSH_AUTH_SOCK", "GIT_ASKPASS"] {
            assert!(
                !args.contains(&key.to_string()),
                "{key} reached the child env"
            );
        }
    }

    #[test]
    fn bwrap_hides_fake_secrets_and_ssh_and_rejects_writes_outside_the_work_tree() {
        if !Path::new(BWRAP_BINARY).is_file() {
            eprintln!("skipping bwrap filesystem test: /usr/bin/bwrap is not installed");
            return;
        }

        let root = tempfile::TempDir::new().expect("temp directory");
        let work_tree = root.path().join("state/e2e/repo");
        let scratch = root.path().join("state/scratch/lab");
        let git_dir = work_tree.join(".git");
        let private = root.path().join("private");
        let secrets = private.join("secrets/token");
        let host_home = private.join("home");
        let ssh_dir = host_home.join(".ssh");
        let credentials = host_home.join(".claude/.credentials.json");
        fs::create_dir_all(&work_tree).expect("work tree");
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(&work_tree)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .expect("run local test git");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet"]);
        fs::write(work_tree.join("tracked.txt"), "committed\n").expect("tracked file");
        git(&["add", "tracked.txt"]);
        git(&[
            "-c",
            "user.name=Sandbox Test",
            "-c",
            "user.email=sandbox@example.invalid",
            "commit",
            "-q",
            "-m",
            "initial",
        ]);
        fs::create_dir_all(git_dir.join("evil/info")).expect("fake common metadata");
        fs::create_dir_all(scratch.join("home")).expect("scratch home");
        fs::create_dir_all(&ssh_dir).expect("fake ssh dir");
        fs::create_dir_all(credentials.parent().expect("credentials parent"))
            .expect("fake claude dir");
        fs::create_dir_all(secrets.parent().expect("secrets parent")).expect("fake secrets dir");
        fs::write(&secrets, b"fake").expect("fake secret");
        fs::write(&credentials, b"fake").expect("fake credentials");
        fs::write(ssh_dir.join("id_fake"), b"fake").expect("fake ssh key");

        let outside = PathBuf::from(format!("/etc/jgf-bwrap-{}-outside", std::process::id()));
        let spec = BwrapSpec {
            work_tree: work_tree.clone(),
            scratch: scratch.clone(),
            credentials_file: credentials,
            read_only_runtime_dirs: Vec::new(),
            env: vec![
                ("HOME".into(), scratch.join("home").into_os_string()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ],
            child_argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                concat!(
                    "test \"$HOME\" = \"$1\" || exit 41; ",
                    "test ! -e \"$2\" || exit 42; ",
                    "test ! -e \"$3\" || exit 43; ",
                    "test ! -e \"$HOME/.ssh\" || exit 44; ",
                    "git status --short >/dev/null || exit 47; ",
                    "git diff --exit-code >/dev/null || exit 48; ",
                    "git log -1 --oneline >/dev/null || exit 49; ",
                    "touch \"$4/.git/commondir\" 2>/dev/null && exit 50; ",
                    "printf x > \"$4/.git/evil/config\" 2>/dev/null && exit 51; ",
                    "printf x > \"$4/.git/evil/info/attributes\" 2>/dev/null && exit 52; ",
                    "touch \"$4/inside\" || exit 45; ",
                    "touch \"$5\" 2>/dev/null && exit 46; ",
                    "exit 0"
                )
                .into(),
                "bwrap-test".into(),
                scratch.join("home").into_os_string(),
                secrets.into_os_string(),
                ssh_dir.into_os_string(),
                work_tree.clone().into_os_string(),
                outside.clone().into_os_string(),
            ],
        };
        let output = Command::new(BWRAP_BINARY)
            .args(bwrap_args(&spec))
            .output()
            .expect("start bwrap");

        assert!(
            output.status.success(),
            "bwrap child failed ({:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(work_tree.join("inside").is_file());
        assert!(!git_dir.join("commondir").exists());
        assert!(!git_dir.join("evil/config").exists());
        assert!(!git_dir.join("evil/info/attributes").exists());
        assert!(!outside.exists(), "sandbox wrote outside the work tree");
    }

    #[test]
    fn sandbox_rejects_a_missing_non_directory_or_symlink_git_directory() {
        let root = tempfile::TempDir::new().expect("temp directory");
        let work_tree = root.path().join("repo");
        fs::create_dir(&work_tree).expect("work tree");
        let git_dir = work_tree.join(".git");

        assert!(validate_git_directory(&work_tree).is_err());

        fs::write(&git_dir, "not a git directory").expect("git file");
        assert!(validate_git_directory(&work_tree).is_err());

        fs::remove_file(&git_dir).expect("remove git file");
        let target = root.path().join("git-target");
        fs::create_dir(&target).expect("git target");
        std::os::unix::fs::symlink(&target, &git_dir).expect("git symlink");
        assert!(validate_git_directory(&work_tree).is_err());

        fs::remove_file(&git_dir).expect("remove git symlink");
        fs::create_dir(&git_dir).expect("git directory");
        assert!(validate_git_directory(&work_tree).is_ok());
    }

    #[test]
    fn namespace_startup_errors_are_clear_and_do_not_fall_back() {
        let error = bwrap_start_error("bwrap: Creating new namespace: Operation not permitted")
            .expect("namespace error");
        assert!(error.contains("bwrap could not start"));
        assert!(error.contains("Operation not permitted"));
        assert!(bwrap_start_error("ordinary Claude failure").is_none());
    }
}
