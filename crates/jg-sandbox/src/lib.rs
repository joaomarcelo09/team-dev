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
//!
//! # Scoped execs and services
//!
//! Every scoped exec gets its own nested PID namespace and its own private
//! `/tmp`. A daemon therefore never outlives the exec that started it (the
//! kernel kills the exec's process tree when its PID 1 exits), and two execs
//! of the same project cannot see each other's `/tmp` sockets. The throwaway
//! PostgreSQL and Redis instances are hosted in one long-lived supervisor exec
//! instead: [`lifecycle::start_services`] keeps that exec alive for as long as
//! its [`lifecycle::ServiceSet`] exists, waits for the ready markers in the
//! sandbox-wide `/run/jg/services` and stops it on
//! [`lifecycle::ServiceSet::stop`].

pub mod backend;
mod cgroup;
pub mod egress;
pub mod error;
pub mod exec;
pub mod handle;
pub mod lifecycle;
pub mod namespace;
pub mod scope;
pub mod services;
pub mod spec;
mod state;
pub mod toolchain;

pub use backend::{Backend, FullAllowPermit, authorize_full_allow};
pub use egress::{
    AddressClass, Connector, EgressEvent, EgressPolicy, EgressProxy, Resolver, SystemResolver,
    TcpConnector, classify_address,
};
pub use error::{Result, SandboxError};
pub use exec::{ExecOutput, ExecRequest};
pub use handle::{NamespaceIds, SandboxHandle, Verified};
pub use lifecycle::{
    CONTROL_DIR, DEFAULT_READY_TIMEOUT, DEFAULT_SERVICE_LIFETIME, ServiceSet, start_services,
    start_services_with,
};
pub use namespace::NamespaceBackend;
pub use scope::{DEFAULT_SCOPED_TMP_BYTES, ExecScope, ProjectLimits, ScopeAccess};
pub use services::{
    PostgresBinaries, RedisBinaries, ServiceBinaries, ServiceKind, ServicePaths, ServicePrograms,
    ServiceSpec,
};
pub use spec::{
    DEFAULT_EGRESS_PORT, DEFAULT_HOSTNAME, DiskLimits, EgressSpec, LabLimits, SandboxSpec,
    default_ro_binds,
};
pub use toolchain::{PackageManager, PackageManagerKind, TauriLibs, Toolchain};
