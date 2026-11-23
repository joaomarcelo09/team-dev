//! Persistence and verification of a sandbox's identity.

use std::path::{Path, PathBuf};

use nix::sys::signal::kill;
use nix::unistd::Pid;

use crate::error::{Result, SandboxError};
use crate::handle::{NamespaceIds, Verified};
use crate::spec::SandboxSpec;

/// Name of the state file inside a sandbox state directory.
pub(crate) const STATE_FILE: &str = "state.json";
/// Name of the host directory used as the sandbox rootfs mount point.
pub(crate) const ROOTFS_DIR: &str = "rootfs";

/// Everything persisted so another process can re-attach.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct StateFile {
    /// Host pid of the sandbox init.
    pub pid: i32,
    /// The spec the sandbox was created with.
    pub spec: SandboxSpec,
    /// Namespace inode ids at creation time.
    pub namespaces: NamespaceIds,
    /// Host path of the sandbox cgroup, when limits were requested.
    #[serde(default)]
    pub cgroup: Option<PathBuf>,
}

pub(crate) fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_FILE)
}

pub(crate) fn rootfs_path(state_dir: &Path) -> PathBuf {
    state_dir.join(ROOTFS_DIR)
}

/// Write the state file atomically.
pub(crate) fn save(state_dir: &Path, state: &StateFile) -> Result<()> {
    let path = state_path(state_dir);
    let tmp = state_dir.join(format!("{STATE_FILE}.tmp"));
    let text =
        serde_json::to_string_pretty(state).map_err(|source| SandboxError::CorruptState {
            path: path.clone(),
            source,
        })?;
    std::fs::write(&tmp, text).map_err(|source| SandboxError::io(&tmp, source))?;
    std::fs::rename(&tmp, &path).map_err(|source| SandboxError::io(&path, source))
}

/// Load the state file, or `None` when the sandbox was never created or was destroyed.
pub(crate) fn load_optional(state_dir: &Path) -> Result<Option<StateFile>> {
    let path = state_path(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SandboxError::io(&path, source)),
    };
    let state = serde_json::from_str(&text).map_err(|source| SandboxError::CorruptState {
        path: path.clone(),
        source,
    })?;
    Ok(Some(state))
}

/// Load the state file and fail closed when it is missing.
pub(crate) fn load_required(state_dir: &Path) -> Result<StateFile> {
    load_optional(state_dir)?.ok_or_else(|| SandboxError::NotInitialized(state_dir.to_path_buf()))
}

/// Whether a host pid is alive and not a zombie.
pub(crate) fn is_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    if kill(Pid::from_raw(pid), None).is_err() {
        return false;
    }
    !is_zombie(pid)
}

fn is_zombie(pid: i32) -> bool {
    let path = format!("/proc/{pid}/stat");
    let Ok(stat) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Some((_, rest)) = stat.rsplit_once(')') else {
        return false;
    };
    matches!(
        rest.trim_start().chars().next(),
        Some('Z') | Some('X') | Some('x')
    )
}

/// Verify that the recorded init still owns the recorded namespaces.
pub(crate) fn verify_state(state: &StateFile) -> Result<Verified> {
    if !is_alive(state.pid) {
        return Err(SandboxError::not_verified(format!(
            "init process {} is not alive",
            state.pid
        )));
    }
    let namespaces = NamespaceIds::read(state.pid).map_err(|_| {
        SandboxError::not_verified(format!(
            "namespace ids of process {} are not readable",
            state.pid
        ))
    })?;
    if namespaces != state.namespaces {
        return Err(SandboxError::not_verified(
            "namespace ids do not match the recorded sandbox state",
        ));
    }
    if let Some(cgroup) = &state.cgroup
        && !crate::cgroup::pid_inside(cgroup, state.pid)
    {
        return Err(SandboxError::not_verified(format!(
            "init process {} is not in the recorded cgroup {}",
            state.pid,
            cgroup.display()
        )));
    }
    Ok(Verified::new(state.pid, namespaces))
}

/// Remove the state file and the (empty) rootfs mount point.
pub(crate) fn remove_state(state_dir: &Path) -> Result<()> {
    let path = state_path(state_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(SandboxError::io(&path, source)),
    }

    let rootfs = rootfs_path(state_dir);
    match std::fs::remove_dir_all(&rootfs) {
        Ok(()) => {}
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => return Err(SandboxError::io(&rootfs, source)),
    }

    // Best effort: remove the state directory only if nothing else lives there.
    let _ = std::fs::remove_dir(state_dir);
    Ok(())
}
