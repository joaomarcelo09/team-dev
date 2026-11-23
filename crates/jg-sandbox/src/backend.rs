//! The backend boundary and the full-allow permit gate.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::exec::{ExecOutput, ExecRequest};
use crate::handle::{NamespaceIds, SandboxHandle, Verified};
use crate::namespace::NamespaceBackend;
use crate::spec::SandboxSpec;

/// The operations a sandbox implementation provides.
///
/// Every operation is synchronous: the caller blocks while the command runs.
pub trait Backend {
    /// Create and start a fresh sandbox for `spec`.
    fn create(&self, spec: &SandboxSpec) -> Result<SandboxHandle>;

    /// Re-attach to a sandbox created by an earlier process.
    fn attach(&self, state_dir: &Path) -> Result<SandboxHandle>;

    /// Run one command inside the sandbox.
    fn exec(&self, handle: &SandboxHandle, request: &ExecRequest) -> Result<ExecOutput>;

    /// Freshly verify that the sandbox still exists and owns the recorded namespaces.
    fn verify(&self, handle: &SandboxHandle) -> Result<Verified>;

    /// Stop and remove the sandbox. Leaves the lab directory untouched.
    fn destroy(&self, handle: &SandboxHandle) -> Result<()>;
}

/// Proof that a sandbox was verified by a successful, fresh verification.
///
/// There is no public constructor: the only way to obtain one is
/// [`authorize_full_allow`]. The permit records the state directory and the
/// namespace ids it was verified against, so a launcher can check that it
/// belongs to the sandbox it is about to use. A permit is intentionally not
/// `Clone`, so every use requires a new verification of the live sandbox.
#[derive(Debug)]
pub struct FullAllowPermit {
    init_pid: i32,
    state_dir: PathBuf,
    namespaces: NamespaceIds,
}

impl FullAllowPermit {
    pub(crate) fn new(state_dir: PathBuf, verified: Verified) -> Self {
        Self {
            init_pid: verified.init_pid(),
            state_dir,
            namespaces: verified.namespaces(),
        }
    }

    /// Host pid of the init that was verified.
    pub fn init_pid(&self) -> i32 {
        self.init_pid
    }

    /// Host state directory of the sandbox that was verified.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Namespace inode ids the sandbox was verified against.
    pub fn namespaces(&self) -> NamespaceIds {
        self.namespaces
    }
}

/// Verify the sandbox right now and return a permit for a full-allow worker.
///
/// Only the concrete [`NamespaceBackend`] can issue a permit, and its
/// verification runs against the sandbox's persisted state. Fails closed: a
/// missing, dead, destroyed or tampered sandbox yields no permit.
pub fn authorize_full_allow(
    backend: &NamespaceBackend,
    handle: &SandboxHandle,
) -> Result<FullAllowPermit> {
    let verified = backend.verify(handle)?;
    Ok(FullAllowPermit::new(
        handle.state_dir().to_path_buf(),
        verified,
    ))
}
