//! Minimal cgroup v2 helpers for lab and per-project resource limits.
//!
//! Everything here is plain filesystem work on the cgroup v2 hierarchy. A
//! sandbox gets one child cgroup below a writable delegated parent; the init
//! lives in an `init` child so the lab can hand controllers down to one
//! `project-<name>` child per project, and `destroy` empties them all again.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::error::{Result, SandboxError};
use crate::scope::{ProjectLimits, validate_project_name};
use crate::spec::LabLimits;

/// Where cgroup v2 is mounted by default.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Period used for every `cpu.max` quota, in microseconds.
const CPU_PERIOD: u64 = 100_000;
/// Poll interval while waiting for a cgroup to become empty.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How long `remove` waits for a killed cgroup to become empty.
const EMPTY_TIMEOUT: Duration = Duration::from_secs(10);
/// Child cgroup that holds lab-wide processes (the init and unscoped execs).
///
/// Enabling controllers in the lab's `cgroup.subtree_control` needs the lab
/// cgroup itself to be empty, so its own processes live one level below.
const LAB_INIT: &str = "init";
/// Prefix of the per-project cgroups below the lab cgroup.
const PROJECT_PREFIX: &str = "project-";

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

/// Create a child cgroup below a writable delegated parent and apply `limits`.
///
/// The cgroup directory is removed again when any limit cannot be written, so a
/// failed call leaves nothing behind.
pub(crate) fn create(parent: Option<&Path>, limits: &LabLimits) -> Result<PathBuf> {
    let needed = needed_controllers(limits);
    let parent = find_parent(parent, &needed)?;
    let path = parent.join(unique_name());
    fs::create_dir(&path).map_err(|source| SandboxError::io(&path, source))?;

    let prepared = write_limits(&path, limits).and_then(|()| prepare_lab(&path, &needed));
    match prepared {
        Ok(()) => Ok(path),
        Err(error) => {
            remove_quiet(&path);
            Err(error)
        }
    }
}

/// The cgroup lab-wide processes join: the init child, or the lab itself for a
/// lab created before that child existed.
pub(crate) fn member_path(lab: &Path) -> PathBuf {
    let init = lab.join(LAB_INIT);
    if init.is_dir() {
        init
    } else {
        lab.to_path_buf()
    }
}

/// Create or reuse the cgroup of `project` below `lab` and apply its limits.
///
/// A project limit above the matching lab limit is rejected before anything is
/// created, and every controller failure is an error: a project never silently
/// runs with a wider limit than the caller asked for.
pub(crate) fn ensure_project(lab: &Path, project: &str, limits: &ProjectLimits) -> Result<PathBuf> {
    validate_project_name(project)?;
    if !lab.is_dir() {
        return Err(SandboxError::cgroup(
            lab,
            "the lab has no cgroup to hold project limits",
        ));
    }

    let values = project_values(limits);
    let needed = needed_controllers(&values);
    if !needed.is_empty() {
        let parent = lab
            .parent()
            .ok_or_else(|| SandboxError::cgroup(lab, "the lab cgroup has no parent"))?;
        // The lab can only hand a controller down when its own parent delegated
        // it, even if the lab itself did not need that controller for its limits.
        prepare_parent(parent, &needed).map_err(|reason| {
            SandboxError::cgroup(
                lab,
                format!("lab parent {} is not usable: {reason}", parent.display()),
            )
        })?;
    }
    prepare_lab(lab, &needed)?;
    check_project_within_lab(lab, project, limits)?;

    let path = lab.join(format!("{PROJECT_PREFIX}{project}"));
    let created = match fs::create_dir(&path) {
        Ok(()) => true,
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists => false,
        Err(source) => return Err(SandboxError::io(&path, source)),
    };
    if !path.is_dir() {
        return Err(SandboxError::cgroup(
            &path,
            "project cgroup path is not a directory",
        ));
    }

    if let Err(error) = write_limits(&path, &values) {
        if created {
            remove_quiet(&path);
        }
        return Err(error);
    }
    Ok(path)
}

/// Move `pid` into the cgroup at `path`.
pub(crate) fn add_pid(path: &Path, pid: i32) -> Result<()> {
    let procs = path.join("cgroup.procs");
    write_file(&procs, format!("{pid}\n"))
}

/// Kill every process in the cgroup, using `cgroup.kill` when the kernel has it.
pub(crate) fn kill_all(path: &Path) -> Result<()> {
    if fs::write(path.join("cgroup.kill"), "1").is_ok() {
        return Ok(());
    }
    for pid in pids(path)? {
        // A pid may already be gone; the caller waits for the cgroup to empty.
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    Ok(())
}

/// Kill the cgroup, wait for it to become empty and remove it with its children.
///
/// A directory that is already gone is treated as clean, so destroying a
/// sandbox twice stays safe. Child cgroups (the init and every project) are
/// removed first, because a cgroup cannot be removed while it still has
/// children.
pub(crate) fn remove(path: &Path) -> Result<()> {
    remove_children(path)?;
    kill_all(path)?;
    wait_empty(path, EMPTY_TIMEOUT)?;
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SandboxError::io(path, source)),
    }
}

/// Best-effort [`remove`] used on failure paths, where the first error wins.
pub(crate) fn remove_quiet(path: &Path) {
    if let Ok(children) = child_cgroups(path) {
        for child in children {
            remove_quiet(&child);
        }
    }
    let _ = kill_all(path);
    let _ = wait_empty(path, EMPTY_TIMEOUT);
    let _ = fs::remove_dir(path);
}

fn remove_children(path: &Path) -> Result<()> {
    for child in child_cgroups(path)? {
        remove(&child)?;
    }
    Ok(())
}

fn child_cgroups(path: &Path) -> Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(SandboxError::io(path, source)),
    };
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|entry| entry.is_dir())
        .collect();
    children.sort();
    Ok(children)
}

/// Whether `pid` currently belongs to the cgroup at `path` or one of its
/// descendants (the lab keeps its processes in child cgroups).
pub(crate) fn pid_inside(path: &Path, pid: i32) -> bool {
    let Some(wanted) = relative_cgroup_path(path) else {
        return false;
    };
    let wanted = wanted.trim_end_matches('/');
    let text = match fs::read_to_string(format!("/proc/{pid}/cgroup")) {
        Ok(text) => text,
        Err(_) => return false,
    };
    text.lines().any(|line| {
        line.strip_prefix("0::").is_some_and(|current| {
            let current = current.trim_end_matches('/');
            current == wanted || current.starts_with(&format!("{wanted}/"))
        })
    })
}

fn write_limits(path: &Path, limits: &LabLimits) -> Result<()> {
    if let Some(percent) = limits.cpu_percent {
        let quota = u64::from(percent) * CPU_PERIOD / 100;
        write_file(&path.join("cpu.max"), format!("{quota} {CPU_PERIOD}\n"))?;
    }
    if let Some(bytes) = limits.memory_bytes {
        write_file(&path.join("memory.max"), format!("{bytes}\n"))?;
        match write_file(&path.join("memory.swap.max"), "0\n") {
            Ok(()) => {}
            Err(SandboxError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if let Some(pids) = limits.pids {
        write_file(&path.join("pids.max"), format!("{pids}\n"))?;
    }
    Ok(())
}

fn project_values(limits: &ProjectLimits) -> LabLimits {
    LabLimits {
        cpu_percent: limits.cpu_percent,
        memory_bytes: limits.memory_bytes,
        pids: limits.pids,
    }
}

/// Make sure the lab cgroup has an init child holding its own processes and
/// the `needed` controllers enabled for its children.
///
/// A lab cgroup with processes of its own cannot enable controllers for its
/// children (the cgroup v2 no-internal-process rule), so the init is moved into
/// `init` first; a lab created by an older version is migrated here.
fn prepare_lab(lab: &Path, needed: &[&str]) -> Result<()> {
    let init = lab.join(LAB_INIT);
    match fs::create_dir(&init) {
        Ok(()) => {}
        Err(source) if source.kind() == io::ErrorKind::AlreadyExists && init.is_dir() => {}
        Err(source) => return Err(SandboxError::io(&init, source)),
    }
    for pid in pids(lab)? {
        match add_pid(&init, pid) {
            Ok(()) => {}
            Err(SandboxError::Io { source, .. })
                if source.raw_os_error() == Some(nix::errno::Errno::ESRCH as i32) => {}
            Err(error) => return Err(error),
        }
    }
    if needed.is_empty() {
        return Ok(());
    }
    prepare_parent(lab, needed).map_err(|reason| SandboxError::cgroup(lab, reason))
}

/// Reject a project limit that would be wider than the matching lab limit.
///
/// A missing lab limit (`max`) never blocks; the values are read from the lab
/// cgroup itself, so they always describe the limits actually in force.
fn check_project_within_lab(lab: &Path, project: &str, limits: &ProjectLimits) -> Result<()> {
    if let Some(percent) = limits.cpu_percent
        && let Some((lab_quota, lab_period)) = read_cpu_limit(lab)?
        && cpu_exceeds_lab(percent, lab_quota, lab_period)
    {
        return Err(SandboxError::cgroup(
            lab,
            format!(
                "project {project} cpu_percent {percent} exceeds the lab cpu.max \
                 {lab_quota} {lab_period}"
            ),
        ));
    }
    if let Some(bytes) = limits.memory_bytes
        && let Some(lab_bytes) = read_flat_limit(&lab.join("memory.max"))?
        && bytes > lab_bytes
    {
        return Err(SandboxError::cgroup(
            lab,
            format!(
                "project {project} memory_bytes {bytes} exceeds the lab memory.max \
                 {lab_bytes}"
            ),
        ));
    }
    if let Some(count) = limits.pids
        && let Some(lab_count) = read_flat_limit(&lab.join("pids.max"))?
        && count > lab_count
    {
        return Err(SandboxError::cgroup(
            lab,
            format!("project {project} pids {count} exceeds the lab pids.max {lab_count}"),
        ));
    }
    Ok(())
}

/// Whether a project CPU percentage asks for a larger share than the lab quota.
fn cpu_exceeds_lab(percent: u32, lab_quota: u64, lab_period: u64) -> bool {
    let project_quota = u64::from(percent) * CPU_PERIOD / 100;
    let project_share = u128::from(project_quota) * u128::from(lab_period);
    let lab_share = u128::from(lab_quota) * u128::from(CPU_PERIOD);
    project_share > lab_share
}

/// The lab's `cpu.max` quota and period, or `None` when it is unlimited.
fn read_cpu_limit(lab: &Path) -> Result<Option<(u64, u64)>> {
    let path = lab.join("cpu.max");
    let text = fs::read_to_string(&path).map_err(|source| SandboxError::io(&path, source))?;
    let mut fields = text.split_whitespace();
    let quota = fields
        .next()
        .ok_or_else(|| SandboxError::cgroup(&path, "empty cpu.max"))?;
    if quota == "max" {
        return Ok(None);
    }
    let quota = quota
        .parse::<u64>()
        .map_err(|_| SandboxError::cgroup(&path, format!("invalid cpu.max quota {quota:?}")))?;
    let period = match fields.next() {
        Some(period) => period.parse::<u64>().map_err(|_| {
            SandboxError::cgroup(&path, format!("invalid cpu.max period {period:?}"))
        })?,
        None => CPU_PERIOD,
    };
    Ok(Some((quota, period)))
}

/// A flat limit file (`memory.max`, `pids.max`) or `None` when it is `max`.
fn read_flat_limit(path: &Path) -> Result<Option<u64>> {
    let text = fs::read_to_string(path).map_err(|source| SandboxError::io(path, source))?;
    let text = text.trim();
    if text == "max" {
        return Ok(None);
    }
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| SandboxError::cgroup(path, format!("invalid limit value {text:?}")))
}

/// The controllers a limit set actually needs.
fn needed_controllers(limits: &LabLimits) -> Vec<&'static str> {
    let mut needed = Vec::new();
    if limits.cpu_percent.is_some() {
        needed.push("cpu");
    }
    if limits.memory_bytes.is_some() {
        needed.push("memory");
    }
    if limits.pids.is_some() {
        needed.push("pids");
    }
    needed
}

/// Resolve and prepare the parent cgroup that will hold the sandbox cgroup.
fn find_parent(explicit: Option<&Path>, needed: &[&str]) -> Result<PathBuf> {
    ensure_can_move_out()?;

    if let Some(parent) = explicit {
        prepare_parent(parent, needed).map_err(|reason| {
            SandboxError::invalid(format!(
                "cgroup parent {} is not usable: {reason}",
                parent.display()
            ))
        })?;
        return Ok(parent.to_path_buf());
    }

    for candidate in candidates() {
        if prepare_parent(&candidate, needed).is_ok() {
            return Ok(candidate);
        }
    }

    Err(SandboxError::Unsupported {
        reason: format!(
            "no writable delegated cgroup v2 parent with controllers {needed:?}; \
             set SandboxSpec::cgroup_parent"
        ),
    })
}

/// Candidate parents, closest first: the current process's ancestors, then the
/// writable systemd user service cgroup of the current user.
fn candidates() -> Vec<PathBuf> {
    let mut candidates = ancestors();
    candidates.extend(user_service_candidates());
    candidates
}

/// Moving the init out of its current cgroup needs write access there too, so
/// a process stuck in a non-delegated scope cannot use cgroup limits at all.
fn ensure_can_move_out() -> Result<()> {
    let current = current_cgroup().ok_or_else(|| SandboxError::Unsupported {
        reason: "cannot read the current cgroup from /proc/self/cgroup".to_string(),
    })?;
    let dir = Path::new(CGROUP_ROOT).join(current.trim_start_matches('/'));
    let procs = dir.join("cgroup.procs");
    OpenOptions::new()
        .write(true)
        .open(&procs)
        .map_err(|source| SandboxError::Unsupported {
            reason: format!(
                "current cgroup {} is not delegated to this process ({source}); \
                 run inside a delegated cgroup to use sandbox limits",
                dir.display()
            ),
        })?;
    Ok(())
}

fn ancestors() -> Vec<PathBuf> {
    let Some(current) = current_cgroup() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut ancestor = Path::new(&current).parent();
    while let Some(path) = ancestor {
        if path == Path::new("/") {
            break;
        }
        let relative = path.strip_prefix("/").unwrap_or(path);
        out.push(Path::new(CGROUP_ROOT).join(relative));
        ancestor = path.parent();
    }
    out
}

/// The delegated `user@<uid>.service` cgroup, when this is a systemd host.
fn user_service_candidates() -> Vec<PathBuf> {
    let Some(uid) = current_uid() else {
        return Vec::new();
    };
    let slice = Path::new(CGROUP_ROOT)
        .join("user.slice")
        .join(format!("user-{uid}.slice"));
    let preferred = slice.join(format!("user@{uid}.service"));
    let mut out = Vec::new();
    if preferred.is_dir() {
        out.push(preferred.clone());
    }
    let Ok(entries) = fs::read_dir(&slice) else {
        return out;
    };
    let mut others: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir() && *path != preferred)
        .collect();
    others.sort();
    out.extend(others);
    out
}

/// Verify `dir` has the needed controllers and delegate them to its children.
fn prepare_parent(dir: &Path, needed: &[&str]) -> std::result::Result<(), String> {
    let controllers = read_set(&dir.join("cgroup.controllers"))
        .map_err(|error| format!("cannot read cgroup.controllers: {error}"))?;
    for controller in needed {
        if !controllers.iter().any(|name| name == controller) {
            return Err(format!("controller {controller} is not available"));
        }
    }

    let subtree = dir.join("cgroup.subtree_control");
    let mut file = OpenOptions::new()
        .write(true)
        .open(&subtree)
        .map_err(|error| format!("cannot write cgroup.subtree_control: {error}"))?;
    let request: Vec<String> = needed.iter().map(|name| format!("+{name}")).collect();
    file.write_all(request.join(" ").as_bytes())
        .and_then(|()| file.flush())
        .map_err(|error| format!("cannot enable controllers: {error}"))?;
    drop(file);

    let enabled = read_set(&subtree)
        .map_err(|error| format!("cannot read cgroup.subtree_control: {error}"))?;
    for controller in needed {
        if !enabled.iter().any(|name| name == controller) {
            return Err(format!("controller {controller} was not enabled"));
        }
    }
    Ok(())
}

fn read_set(path: &Path) -> io::Result<Vec<String>> {
    let text = fs::read_to_string(path)?;
    Ok(text.split_whitespace().map(str::to_string).collect())
}

fn current_cgroup() -> Option<String> {
    let text = fs::read_to_string("/proc/self/cgroup").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().to_string())
}

fn current_uid() -> Option<u32> {
    let text = fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|uid| uid.parse().ok())
}

fn relative_cgroup_path(path: &Path) -> Option<String> {
    let relative = path.strip_prefix(CGROUP_ROOT).ok()?;
    Some(format!("/{}", relative.to_string_lossy()))
}

fn pids(path: &Path) -> Result<Vec<i32>> {
    let text = match fs::read_to_string(path.join("cgroup.procs")) {
        Ok(text) => text,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(SandboxError::io(path.join("cgroup.procs"), source)),
    };
    Ok(text
        .lines()
        .filter_map(|line| line.trim().parse::<i32>().ok())
        .collect())
}

fn wait_empty(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if pids(path)?.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(SandboxError::cgroup(
                path,
                format!("did not become empty within {}s", timeout.as_secs()),
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn write_file(path: &Path, text: impl AsRef<[u8]>) -> Result<()> {
    fs::write(path, text).map_err(|source| SandboxError::io(path, source))
}

fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("jg-sandbox-{}-{nanos}-{id}", std::process::id())
}
