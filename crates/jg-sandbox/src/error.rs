//! Errors raised by the sandbox.

use std::path::PathBuf;

use thiserror::Error;

/// Result type used across the sandbox crate.
pub type Result<T> = std::result::Result<T, SandboxError>;

/// Errors produced while creating, entering, verifying or destroying a sandbox.
#[derive(Debug, Error)]
pub enum SandboxError {
    /// A path or value in the [`crate::SandboxSpec`] is not usable.
    #[error("invalid sandbox spec: {reason}")]
    InvalidSpec {
        /// Human-readable reason.
        reason: String,
    },

    /// A file operation failed.
    #[error("io error at {}: {source}", path.display())]
    Io {
        /// Path involved in the failed operation.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A namespace, mount or process system call failed.
    #[error("{context}: {source}")]
    Nix {
        /// What was being attempted.
        context: String,
        /// Kernel error.
        #[source]
        source: nix::errno::Errno,
    },

    /// The host does not allow unprivileged user namespaces.
    #[error("sandboxing is unsupported on this host: {reason}")]
    Unsupported {
        /// Why the feature is unavailable.
        reason: String,
    },

    /// A sandbox already exists in the requested state directory.
    #[error("a sandbox already exists at {}", .0.display())]
    AlreadyExists(PathBuf),

    /// The state directory holds no sandbox.
    #[error("no sandbox state at {}", .0.display())]
    NotInitialized(PathBuf),

    /// The stored sandbox state could not be read.
    #[error("invalid sandbox state at {}: {source}", path.display())]
    CorruptState {
        /// Path of the state file.
        path: PathBuf,
        /// JSON parser error.
        #[source]
        source: serde_json::Error,
    },

    /// The sandbox is no longer the one recorded in its state (dead, destroyed or replaced).
    #[error("sandbox is not verified: {reason}")]
    NotVerified {
        /// Why verification failed.
        reason: String,
    },

    /// The persistent init never became ready.
    #[error("sandbox init failed: {reason}")]
    InitFailed {
        /// What the init reported.
        reason: String,
    },

    /// A command could not be started inside the sandbox.
    #[error("could not run command: {reason}")]
    ExecFailed {
        /// Why the command did not start.
        reason: String,
    },

    /// A process did not stop in time.
    #[error("process {pid} did not stop within {seconds}s")]
    StopTimeout {
        /// Host pid that was waited on.
        pid: i32,
        /// Timeout that elapsed.
        seconds: u64,
    },

    /// A cgroup operation failed.
    #[error("cgroup error at {}: {reason}", path.display())]
    Cgroup {
        /// Cgroup directory involved in the failed operation.
        path: PathBuf,
        /// Human-readable reason.
        reason: String,
    },
}

impl SandboxError {
    /// An I/O error that happened at `path`.
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// A `nix` error with the operation that failed.
    pub(crate) fn nix(context: impl Into<String>, source: nix::errno::Errno) -> Self {
        Self::Nix {
            context: context.into(),
            source,
        }
    }

    /// An invalid-spec error.
    pub(crate) fn invalid(reason: impl Into<String>) -> Self {
        Self::InvalidSpec {
            reason: reason.into(),
        }
    }

    /// A verification failure.
    pub(crate) fn not_verified(reason: impl Into<String>) -> Self {
        Self::NotVerified {
            reason: reason.into(),
        }
    }

    /// A cgroup operation failure.
    pub(crate) fn cgroup(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self::Cgroup {
            path: path.into(),
            reason: reason.into(),
        }
    }
}
