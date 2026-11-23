//! The unprivileged Linux namespace backend.

use std::path::Path;
use std::time::Duration;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::backend::Backend;
use crate::cgroup;
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

        // The cgroup exists with its limits before the init is started at all.
        let cgroup = match &spec.limits {
            Some(limits) => Some(cgroup::create(spec.cgroup_parent.as_deref(), limits)?),
            None => None,
        };

        let created = self.create_with_cgroup(spec, cgroup.as_deref());
        if created.is_err()
            && let Some(path) = &cgroup
        {
            cgroup::remove_quiet(path);
            let _ = state::remove_state(&spec.state_dir);
        }
        created
    }

    fn attach(&self, state_dir: &Path) -> Result<SandboxHandle> {
        let saved = state::load_required(state_dir)?;
        saved.spec.validate()?;
        state::verify_state(&saved)?;
        Ok(SandboxHandle::new(saved.spec, saved.pid, saved.cgroup))
    }

    fn exec(&self, handle: &SandboxHandle, request: &ExecRequest) -> Result<ExecOutput> {
        let saved = self.verified_state(handle)?;
        exec::run(
            saved.pid,
            saved.cgroup.as_deref(),
            saved.spec.disk.tmp_bytes,
            request,
        )
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
            if let Some(path) = &saved.cgroup {
                cgroup::remove(path)?;
            }
            self.stop_init(&saved)?;
        }
        state::remove_state(handle.state_dir())
    }
}

impl NamespaceBackend {
    fn create_with_cgroup(
        &self,
        spec: &SandboxSpec,
        cgroup: Option<&Path>,
    ) -> Result<SandboxHandle> {
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

        // Everything the init and its commands do from now on is accounted to
        // the lab cgroup: the init itself, and every exec worker that inherits
        // this membership. The init lives in the lab's child cgroup so the lab
        // itself can hand controllers down to project cgroups.
        if let Some(path) = cgroup {
            let member = cgroup::member_path(path);
            if let Err(error) = cgroup::add_pid(&member, pid) {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
                let _ = util::wait_dead(pid, STOP_TIMEOUT);
                let _ = state::remove_state(&spec.state_dir);
                return Err(error);
            }
        }

        let prepared = (|| -> Result<()> {
            let namespaces = NamespaceIds::read(pid)?;
            let saved = StateFile {
                pid,
                spec: spec.clone(),
                namespaces,
                cgroup: cgroup.map(Path::to_path_buf),
            };
            state::save(&spec.state_dir, &saved)
        })();
        if let Err(error) = prepared {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            let _ = util::wait_dead(pid, STOP_TIMEOUT);
            let _ = state::remove_state(&spec.state_dir);
            return Err(error);
        }

        Ok(SandboxHandle::new(
            spec.clone(),
            pid,
            cgroup.map(Path::to_path_buf),
        ))
    }

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
