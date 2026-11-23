//! The description of one persistent sandbox world.

use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, SandboxError};

/// Default hostname given to a sandbox when the caller does not choose one.
pub const DEFAULT_HOSTNAME: &str = "jg-sandbox";

/// Everything needed to create a persistent sandbox world.
///
/// All paths are host paths. The lab directory is bind-mounted read-write at
/// `/lab`; every entry of `ro_binds` is mounted read-only at the same absolute
/// path inside the sandbox; every entry of `allowed_sockets` is the only host
/// socket exposed inside.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxSpec {
    /// Host directory holding the sandbox state (init pid, namespace ids, spec).
    pub state_dir: PathBuf,
    /// Host directory bind-mounted read-write at `/lab`.
    pub lab_dir: PathBuf,
    /// Host paths bind-mounted read-only at the same path inside the sandbox.
    pub ro_binds: Vec<PathBuf>,
    /// Host unix socket paths bind-mounted inside (hub shim, broker later).
    pub allowed_sockets: Vec<PathBuf>,
    /// Hostname of the sandbox UTS namespace.
    pub hostname: String,
}

impl SandboxSpec {
    /// A spec with the minimal default read-only toolchain bind list.
    pub fn new(state_dir: impl Into<PathBuf>, lab_dir: impl Into<PathBuf>) -> Self {
        Self {
            state_dir: state_dir.into(),
            lab_dir: lab_dir.into(),
            ro_binds: default_ro_binds(),
            allowed_sockets: Vec::new(),
            hostname: DEFAULT_HOSTNAME.to_string(),
        }
    }

    /// Validate every path before anything is started.
    pub fn validate(&self) -> Result<()> {
        validate_path(&self.state_dir, "state dir")?;
        if self.state_dir.exists() && !self.state_dir.is_dir() {
            return Err(SandboxError::invalid(format!(
                "state dir {} exists but is not a directory",
                self.state_dir.display()
            )));
        }
        validate_dir(&self.lab_dir, "lab dir")?;

        if path_is_root(&self.state_dir) {
            return Err(SandboxError::invalid("state dir must not be /"));
        }
        if path_is_root(&self.lab_dir) {
            return Err(SandboxError::invalid("lab dir must not be /"));
        }
        if self.state_dir.starts_with(&self.lab_dir) {
            return Err(SandboxError::invalid(
                "state dir must not be inside the lab dir",
            ));
        }

        for path in &self.ro_binds {
            validate_path(path, "read-only bind")?;
            if path_is_root(path) {
                return Err(SandboxError::invalid("read-only bind must not be /"));
            }
            if !path.exists() {
                return Err(SandboxError::invalid(format!(
                    "read-only bind {} does not exist",
                    path.display()
                )));
            }
        }

        for path in &self.allowed_sockets {
            validate_path(path, "allowed socket")?;
            let meta = std::fs::symlink_metadata(path).map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    SandboxError::invalid(format!(
                        "allowed socket {} does not exist",
                        path.display()
                    ))
                } else {
                    SandboxError::io(path, source)
                }
            })?;
            if !meta.file_type().is_socket() {
                return Err(SandboxError::invalid(format!(
                    "allowed socket {} is not a unix socket",
                    path.display()
                )));
            }
        }

        validate_hostname(&self.hostname)
    }
}

/// The minimal set of host toolchain directories exposed read-only by default.
pub fn default_ro_binds() -> Vec<PathBuf> {
    let mut binds = vec![PathBuf::from("/usr"), PathBuf::from("/etc/ssl/certs")];
    for path in ["/bin", "/sbin", "/lib", "/lib64"] {
        let path = Path::new(path);
        match std::fs::symlink_metadata(path) {
            Ok(meta) if !meta.file_type().is_symlink() => binds.push(path.to_path_buf()),
            _ => {}
        }
    }
    binds
}

fn validate_dir(path: &Path, what: &str) -> Result<()> {
    validate_path(path, what)?;
    if !path.is_dir() {
        return Err(SandboxError::invalid(format!(
            "{what} {} does not exist or is not a directory",
            path.display()
        )));
    }
    Ok(())
}

fn validate_path(path: &Path, what: &str) -> Result<()> {
    if !path.is_absolute() {
        return Err(SandboxError::invalid(format!(
            "{what} {} is not absolute",
            path.display()
        )));
    }
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(SandboxError::invalid(format!(
            "{what} {} contains ..",
            path.display()
        )));
    }
    Ok(())
}

fn validate_hostname(hostname: &str) -> Result<()> {
    if hostname.is_empty() || hostname.len() > 64 {
        return Err(SandboxError::invalid(
            "hostname must be between 1 and 64 bytes",
        ));
    }
    let valid = hostname
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    if !valid {
        return Err(SandboxError::invalid(
            "hostname may only contain letters, digits, '-' and '.'",
        ));
    }
    Ok(())
}

fn path_is_root(path: &Path) -> bool {
    path.parent().is_none() && path.is_absolute()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_parent_paths_are_rejected() {
        let mut spec = SandboxSpec::new("/tmp/x/state", "/tmp/x/lab");
        spec.state_dir = PathBuf::from("relative");
        assert!(spec.validate().is_err());

        spec.state_dir = PathBuf::from("/tmp/x/state");
        spec.lab_dir = PathBuf::from("/tmp/x/../lab");
        assert!(spec.validate().is_err());
    }

    #[test]
    fn default_binds_are_absolute() {
        for path in default_ro_binds() {
            assert!(path.is_absolute());
        }
    }

    #[test]
    fn invalid_hostnames_are_rejected() {
        assert!(validate_hostname("valid-name").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("bad name").is_err());
    }
}
