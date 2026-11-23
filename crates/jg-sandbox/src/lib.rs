//! Persistent, unprivileged Linux namespace sandbox for Dark Factory.
//!
//! One sandbox is a long-lived "lab world": an init process that is pid 1 of a
//! fresh PID namespace and owns user, mount, IPC, UTS and network namespaces.
//! The host root is replaced with a fresh tmpfs via `pivot_root`, only declared
//! read-only binds and the read-write lab directory are visible, and commands
//! join the init's namespaces with `setns` before they run.
//!
//! Dark Factory is separate from the stable app: this crate is only used by
//! Dark Factory crates.

pub mod backend;
mod cgroup;
pub mod error;
pub mod exec;
pub mod handle;
pub mod namespace;
pub mod scope;
pub mod spec;
mod state;

pub use backend::{Backend, FullAllowPermit, authorize_full_allow};
pub use error::{Result, SandboxError};
pub use exec::{ExecOutput, ExecRequest};
pub use handle::{NamespaceIds, SandboxHandle, Verified};
pub use namespace::NamespaceBackend;
pub use scope::{DEFAULT_SCOPED_TMP_BYTES, ExecScope, ProjectLimits, ScopeAccess};
pub use spec::{DEFAULT_HOSTNAME, DiskLimits, LabLimits, SandboxSpec, default_ro_binds};
