//! Handles and namespace identity for a persistent sandbox.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::error::{Result, SandboxError};
use crate::spec::SandboxSpec;

/// Inode ids of the six namespaces a sandbox owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NamespaceIds {
    /// User namespace inode.
    pub user: u64,
    /// Mount namespace inode.
    pub mount: u64,
    /// PID namespace inode.
    pub pid: u64,
    /// IPC namespace inode.
    pub ipc: u64,
    /// UTS namespace inode.
    pub uts: u64,
    /// Network namespace inode.
    pub net: u64,
}

impl NamespaceIds {
    /// Read the namespace inode ids of the process `pid` as seen from the host.
    pub fn read(pid: i32) -> Result<Self> {
        Ok(Self {
            user: ns_inode(pid, "user")?,
            mount: ns_inode(pid, "mnt")?,
            pid: ns_inode(pid, "pid")?,
            ipc: ns_inode(pid, "ipc")?,
            uts: ns_inode(pid, "uts")?,
            net: ns_inode(pid, "net")?,
        })
    }
}

fn ns_inode(pid: i32, name: &str) -> Result<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/ns/{name}"));
    let meta = std::fs::metadata(&path).map_err(|source| SandboxError::io(&path, source))?;
    Ok(meta.ino())
}

/// A live or recorded sandbox. Cheap to clone and safe to keep in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxHandle {
    spec: SandboxSpec,
    init_pid: i32,
}

impl SandboxHandle {
    pub(crate) fn new(spec: SandboxSpec, init_pid: i32) -> Self {
        Self { spec, init_pid }
    }

    /// Host pid of the sandbox init (pid 1 of the sandbox PID namespace).
    pub fn init_pid(&self) -> i32 {
        self.init_pid
    }

    /// The host directory holding this sandbox's state.
    pub fn state_dir(&self) -> &Path {
        &self.spec.state_dir
    }

    /// The host directory mounted at `/lab`.
    pub fn lab_dir(&self) -> &Path {
        &self.spec.lab_dir
    }

    /// The spec this sandbox was created with.
    pub fn spec(&self) -> &SandboxSpec {
        &self.spec
    }
}

/// The identity of a sandbox whose stored state matches the live process.
///
/// A `Verified` can only be produced by this crate's internal state
/// verification. The fields are private, so no other crate can forge a
/// verification result:
///
/// ```compile_fail
/// use jg_sandbox::{NamespaceIds, Verified};
///
/// let _forged = Verified {
///     init_pid: 1,
///     namespaces: NamespaceIds {
///         user: 0,
///         mount: 0,
///         pid: 0,
///         ipc: 0,
///         uts: 0,
///         net: 0,
///     },
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    init_pid: i32,
    namespaces: NamespaceIds,
}

impl Verified {
    pub(crate) fn new(init_pid: i32, namespaces: NamespaceIds) -> Self {
        Self {
            init_pid,
            namespaces,
        }
    }

    /// Host pid of the verified init.
    pub fn init_pid(&self) -> i32 {
        self.init_pid
    }

    /// Namespace inode ids observed at verification time.
    pub fn namespaces(&self) -> NamespaceIds {
        self.namespaces
    }
}
