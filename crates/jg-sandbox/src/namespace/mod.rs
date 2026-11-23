//! The unprivileged Linux namespace backend.

use std::path::Path;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::backend::Backend;
use crate::error::{Result, SandboxError};
use crate::exec::{ExecOutput, ExecRequest};
use crate::handle::{NamespaceIds, SandboxHandle, Verified};
use crate::spec::SandboxSpec;
use crate::state::{self, StateFile};

mod exec;
mod init;
mod rootfs;
mod scope;
mod util;

const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// A backend built on Linux user, mount, pid, ipc, uts and net namespaces.
#[derive(Debug, Default, Clone, Copy)]
pub struct NamespaceBackend;

impl NamespaceBackend {
    /// Create a backend; it holds no state of its own.
    pub fn new() -> Self {
        Self
    }
}

impl Backend for NamespaceBackend {
    fn create(&self, spec: &SandboxSpec) -> Result<SandboxHandle> {
        spec.validate()?;
        if state::state_path(&spec.state_dir).exists() {
            return Err(SandboxError::AlreadyExists(spec.state_dir.clone()));
        }
        std::fs::create_dir_all(&spec.state_dir)
            .map_err(|source| SandboxError::io(&spec.state_dir, source))?;

        let rootfs = state::rootfs_path(&spec.state_dir);
        if rootfs.exists() {
            return Err(SandboxError::invalid(format!(
                "rootfs path {} already exists",
                rootfs.display()
            )));
        }
        std::fs::create_dir(&rootfs).map_err(|source| SandboxError::io(&rootfs, source))?;

        let pid = match init::spawn(spec) {
            Ok(pid) => pid,
            Err(error) => {
                let _ = state::remove_state(&spec.state_dir);
                return Err(error);
            }
        };

        let prepared = (|| -> Result<()> {
            let namespaces = NamespaceIds::read(pid)?;
            let saved = StateFile {
                pid,
                spec: spec.clone(),
                namespaces,
            };
            state::save(&spec.state_dir, &saved)
        })();
        if let Err(error) = prepared {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            let _ = util::wait_dead(pid, STOP_TIMEOUT);
            let _ = state::remove_state(&spec.state_dir);
            return Err(error);
        }

        Ok(SandboxHandle::new(spec.clone(), pid))
    }

    fn attach(&self, state_dir: &Path) -> Result<SandboxHandle> {
        let saved = state::load_required(state_dir)?;
        saved.spec.validate()?;
        state::verify_state(&saved)?;
        Ok(SandboxHandle::new(saved.spec, saved.pid))
    }

    fn exec(&self, handle: &SandboxHandle, request: &ExecRequest) -> Result<ExecOutput> {
        let saved = self.verified_state(handle)?;
        exec::run(saved.pid, request)
    }

    fn verify(&self, handle: &SandboxHandle) -> Result<Verified> {
        let saved = state::load_required(handle.state_dir())?;
        if saved.pid != handle.init_pid() {
            return Err(SandboxError::not_verified(
                "handle does not match the recorded sandbox state",
            ));
        }
        state::verify_state(&saved)
    }

    fn destroy(&self, handle: &SandboxHandle) -> Result<()> {
        if let Some(saved) = state::load_optional(handle.state_dir())? {
            self.stop_init(&saved)?;
        }
        state::remove_state(handle.state_dir())
    }
}

impl NamespaceBackend {
    fn verified_state(&self, handle: &SandboxHandle) -> Result<StateFile> {
        let saved = state::load_required(handle.state_dir())?;
        if saved.pid != handle.init_pid() {
            return Err(SandboxError::not_verified(
                "handle does not match the recorded sandbox state",
            ));
        }
        state::verify_state(&saved)?;
        Ok(saved)
    }

    fn stop_init(&self, saved: &StateFile) -> Result<()> {
        if !state::is_alive(saved.pid) {
            return Ok(());
        }
        // Only signal when the pid still owns the recorded PID namespace, so a
        // recycled host pid is never killed.
        match NamespaceIds::read(saved.pid) {
            Ok(namespaces) if namespaces == saved.namespaces => {}
            _ => return Ok(()),
        }
        let _ = kill(Pid::from_raw(saved.pid), Signal::SIGKILL);
        util::wait_dead(saved.pid, STOP_TIMEOUT)
    }
}
