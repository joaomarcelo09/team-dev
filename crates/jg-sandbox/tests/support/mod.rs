//! Shared helpers for the jg-sandbox integration tests.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use jg_sandbox::{
    Backend, ExecOutput, ExecRequest, NamespaceBackend, SandboxError, SandboxHandle, SandboxSpec,
};
use tempfile::TempDir;

/// A backend plus a sandbox that is destroyed when the guard drops.
pub struct SandboxGuard {
    backend: NamespaceBackend,
    handle: SandboxHandle,
}

impl SandboxGuard {
    /// The live sandbox handle.
    pub fn handle(&self) -> &SandboxHandle {
        &self.handle
    }
}

impl Drop for SandboxGuard {
    fn drop(&mut self) {
        let _ = self.backend.destroy(&self.handle);
    }
}

/// A temporary lab root and its spec: `<tmp>/state` and `<tmp>/lab`.
pub struct LabFixture {
    /// Keeps the temporary directory alive for the test.
    pub temp: TempDir,
    /// Spec pointing inside the temporary directory.
    pub spec: SandboxSpec,
}

/// Build a temporary lab with an existing lab directory.
pub fn fixture() -> LabFixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let lab = temp.path().join("lab");
    std::fs::create_dir_all(&lab).expect("lab dir");
    let spec = SandboxSpec::new(temp.path().join("state"), lab);
    LabFixture { temp, spec }
}

/// Marker env var: the test body is already running in a re-executed child.
const MARKER_ENV: &str = "JG_CGROUP_TEST_MARKER";

/// Cgroup limits need the caller to be inside a delegated cgroup; when the test
/// binary runs in a plain session scope, re-run this single test under a
/// transient user scope so the kernel allows moving processes into the lab
/// cgroup. Returns false only when delegation is truly unavailable.
pub fn ensure_delegated(test_name: &str) -> bool {
    // This is the re-executed child: record that the harness ran, then require
    // a delegated cgroup from here on.
    if let Some(marker) = std::env::var_os(MARKER_ENV) {
        let _ = fs::write(marker, "started");
        assert!(
            current_cgroup_writable(),
            "cgroup test {test_name} started without a delegated cgroup"
        );
        return true;
    }

    if current_cgroup_writable() {
        return true;
    }

    let marker = std::env::temp_dir().join(format!(
        "jg-cgroup-reexec-{}-{test_name}",
        std::process::id()
    ));
    let _ = fs::remove_file(&marker);
    let exe = std::env::current_exe().expect("test binary path");
    let status = Command::new("systemd-run")
        .args(["--user", "--scope", "--collect", "--quiet", "--"])
        .arg(&exe)
        .args(["--exact", test_name, "--nocapture"])
        .env(MARKER_ENV, &marker)
        .status();
    let started = marker.exists();
    let _ = fs::remove_file(&marker);

    match status {
        Ok(status) if started && status.success() => false,
        Ok(status) if started => panic!("delegated cgroup test {test_name} failed: {status}"),
        _ => {
            eprintln!(
                "skipping cgroup test {test_name}: cgroup delegation is unavailable to this process"
            );
            false
        }
    }
}

fn current_cgroup_writable() -> bool {
    let Ok(text) = fs::read_to_string("/proc/self/cgroup") else {
        return false;
    };
    let Some(relative) = text.lines().find_map(|line| line.strip_prefix("0::")) else {
        return false;
    };
    let dir = Path::new("/sys/fs/cgroup").join(relative.trim_start_matches('/'));
    fs::OpenOptions::new()
        .write(true)
        .open(dir.join("cgroup.procs"))
        .is_ok()
}

/// Create a sandbox, or return `None` when the host cannot run unprivileged namespaces.
pub fn create_or_skip(backend: NamespaceBackend, spec: &SandboxSpec) -> Option<SandboxGuard> {
    match backend.create(spec) {
        Ok(handle) => Some(SandboxGuard { backend, handle }),
        Err(SandboxError::Unsupported { reason }) => {
            eprintln!("skipping sandbox test: {reason}");
            None
        }
        Err(error) => panic!("create failed: {error}"),
    }
}

/// Run a shell script inside the sandbox and return its output.
pub fn run_shell(backend: NamespaceBackend, handle: &SandboxHandle, script: &str) -> ExecOutput {
    let request = ExecRequest::new(vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        script.to_string(),
    ])
    .with_timeout(Duration::from_secs(30));
    match backend.exec(handle, &request) {
        Ok(output) => output,
        Err(error) => panic!("exec failed: {error}"),
    }
}

/// Run one command and require a zero exit code.
pub fn run_ok(backend: NamespaceBackend, handle: &SandboxHandle, script: &str) -> ExecOutput {
    let output = run_shell(backend, handle, script);
    assert_eq!(
        output.exit_code,
        Some(0),
        "command failed: {}",
        output.stderr_text()
    );
    output
}

/// Assert that a host pid disappears within a few seconds.
pub fn assert_process_gone(pid: i32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while process_exists(pid) {
        assert!(
            Instant::now() < deadline,
            "process {pid} is still alive after destroy"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn process_exists(pid: i32) -> bool {
    let path = PathBuf::from(format!("/proc/{pid}"));
    if !path.exists() {
        return false;
    }
    !is_zombie(pid)
}

/// Whether `pid` exists but is already a zombie.
pub fn is_zombie(pid: i32) -> bool {
    let stat = PathBuf::from(format!("/proc/{pid}/stat"));
    let Ok(text) = std::fs::read_to_string(stat) else {
        return false;
    };
    let Some((_, rest)) = text.rsplit_once(')') else {
        return false;
    };
    matches!(
        rest.trim_start().chars().next(),
        Some('Z') | Some('X') | Some('x')
    )
}

/// Inode of the PID namespace of a host pid.
pub fn pid_namespace_inode(pid: i32) -> u64 {
    namespace_inode(pid, "pid")
}

/// Inode of the mount namespace of a host pid.
pub fn mount_namespace_inode(pid: i32) -> u64 {
    namespace_inode(pid, "mnt")
}

fn namespace_inode(pid: i32, name: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let path = PathBuf::from(format!("/proc/{pid}/ns/{name}"));
    std::fs::metadata(path)
        .map(|meta| meta.ino())
        .unwrap_or_default()
}

/// Live host pids that are members of the PID namespace with `inode`.
pub fn pids_in_pid_namespace(inode: u64) -> Vec<i32> {
    pids_in_namespace("pid", inode)
}

/// Live host pids that are members of the mount namespace with `inode`.
pub fn pids_in_mount_namespace(inode: u64) -> Vec<i32> {
    pids_in_namespace("mnt", inode)
}

fn pids_in_namespace(name: &str, inode: u64) -> Vec<i32> {
    use std::os::unix::fs::MetadataExt;
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let path = PathBuf::from(format!("/proc/{pid}/ns/{name}"));
        let Ok(meta) = std::fs::metadata(path) else {
            continue;
        };
        if meta.ino() == inode && !is_zombie(pid) {
            pids.push(pid);
        }
    }
    pids
}

/// The command line of a host pid, split on NUL bytes.
pub fn process_args(pid: i32) -> Vec<String> {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// Retry `ready` until it returns true, or panic after a few seconds.
pub fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
