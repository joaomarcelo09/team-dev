//! The description of one persistent sandbox world.

use std::os::unix::fs::FileTypeExt;
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, SandboxError};

/// Default hostname given to a sandbox when the caller does not choose one.
pub const DEFAULT_HOSTNAME: &str = "jg-sandbox";

/// Smallest accepted disk cap (1 MiB); anything below is a configuration
/// mistake, not a usable sandbox.
pub const MIN_DISK_BYTES: u64 = 1024 * 1024;

/// Default size of the sandbox root tmpfs (512 MiB).
pub const DEFAULT_ROOT_DISK_BYTES: u64 = 512 * 1024 * 1024;

/// Default size of the sandbox `/tmp` tmpfs (1 GiB).
pub const DEFAULT_TMP_DISK_BYTES: u64 = 1024 * 1024 * 1024;

/// Size caps for the two tmpfs mounts of a sandbox.
///
/// The defaults keep the sizes of earlier releases, so a state file written
/// before these fields existed still describes the same sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DiskLimits {
    /// Size of the root tmpfs in bytes.
    pub root_bytes: u64,
    /// Size of the shared lab-wide `/tmp` tmpfs in bytes.
    pub tmp_bytes: u64,
}

impl Default for DiskLimits {
    fn default() -> Self {
        Self {
            root_bytes: DEFAULT_ROOT_DISK_BYTES,
            tmp_bytes: DEFAULT_TMP_DISK_BYTES,
        }
    }
}

/// Lab-level resource limits applied through cgroup v2.
///
/// Every field is optional; a missing value leaves that resource unlimited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct LabLimits {
    /// CPU quota where 100 means one full core.
    #[serde(default)]
    pub cpu_percent: Option<u32>,
    /// Memory limit in bytes.
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    /// Maximum number of tasks in the sandbox cgroup.
    #[serde(default)]
    pub pids: Option<u64>,
}

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
    /// Optional cgroup v2 resource limits for the whole lab.
    #[serde(default)]
    pub limits: Option<LabLimits>,
    /// Size caps of the root and `/tmp` tmpfs mounts.
    #[serde(default)]
    pub disk: DiskLimits,
    /// Delegated cgroup v2 parent; discovered when absent.
    #[serde(default)]
    pub cgroup_parent: Option<PathBuf>,
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
            limits: None,
            disk: DiskLimits::default(),
            cgroup_parent: None,
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

        if let Some(limits) = &self.limits {
            validate_limits(limits)?;
        }
        validate_disk(&self.disk)?;
        if let Some(parent) = &self.cgroup_parent {
            validate_path(parent, "cgroup parent")?;
        }

        validate_hostname(&self.hostname)
    }
}

fn validate_disk(disk: &DiskLimits) -> Result<()> {
    for (name, bytes) in [
        ("root_bytes", disk.root_bytes),
        ("tmp_bytes", disk.tmp_bytes),
    ] {
        if bytes < MIN_DISK_BYTES {
            return Err(SandboxError::invalid(format!(
                "{name} must be at least {MIN_DISK_BYTES} bytes, got {bytes}"
            )));
        }
    }
    Ok(())
}

fn validate_limits(limits: &LabLimits) -> Result<()> {
    if limits.cpu_percent == Some(0) {
        return Err(SandboxError::invalid(
            "cpu_percent must be greater than zero",
        ));
    }
    if limits.memory_bytes == Some(0) {
        return Err(SandboxError::invalid(
            "memory_bytes must be greater than zero",
        ));
    }
    if limits.pids == Some(0) {
        return Err(SandboxError::invalid("pids must be greater than zero"));
    }
    Ok(())
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

    #[test]
    fn zero_limits_and_relative_cgroup_parent_are_rejected() {
        let mut spec = SandboxSpec::new("/tmp/x/state", "/tmp/x/lab");
        spec.limits = Some(LabLimits {
            cpu_percent: Some(0),
            memory_bytes: None,
            pids: None,
        });
        assert!(spec.validate().is_err());

        spec.limits = Some(LabLimits {
            cpu_percent: Some(50),
            memory_bytes: Some(0),
            pids: None,
        });
        assert!(spec.validate().is_err());

        spec.limits = Some(LabLimits {
            cpu_percent: Some(50),
            memory_bytes: Some(1024),
            pids: Some(0),
        });
        assert!(spec.validate().is_err());

        spec.limits = None;
        spec.cgroup_parent = Some(PathBuf::from("relative/cgroup"));
        assert!(spec.validate().is_err());
    }

    #[test]
    fn old_state_without_limits_still_deserializes() {
        let json = r#"{
            "state_dir": "/tmp/x/state",
            "lab_dir": "/tmp/x/lab",
            "ro_binds": [],
            "allowed_sockets": [],
            "hostname": "jg-sandbox"
        }"#;
        let spec: SandboxSpec = serde_json::from_str(json).expect("old state loads");
        assert_eq!(spec.limits, None);
        assert_eq!(spec.disk, DiskLimits::default());
        assert_eq!(spec.cgroup_parent, None);
    }

    #[test]
    fn disk_defaults_keep_the_old_tmpfs_sizes() {
        assert_eq!(DEFAULT_ROOT_DISK_BYTES, 512 * 1024 * 1024);
        assert_eq!(DEFAULT_TMP_DISK_BYTES, 1024 * 1024 * 1024);
        assert_eq!(
            DiskLimits::default(),
            DiskLimits {
                root_bytes: 512 * 1024 * 1024,
                tmp_bytes: 1024 * 1024 * 1024,
            }
        );
    }

    #[test]
    fn disk_limits_below_the_minimum_are_rejected() {
        let mut disk = DiskLimits::default();
        for bytes in [0, MIN_DISK_BYTES - 1] {
            disk.root_bytes = bytes;
            assert!(
                validate_disk(&disk).is_err(),
                "root_bytes {bytes} must be rejected"
            );
            disk.root_bytes = DEFAULT_ROOT_DISK_BYTES;
            disk.tmp_bytes = bytes;
            assert!(
                validate_disk(&disk).is_err(),
                "tmp_bytes {bytes} must be rejected"
            );
            disk.tmp_bytes = DEFAULT_TMP_DISK_BYTES;
        }
        disk.root_bytes = MIN_DISK_BYTES;
        disk.tmp_bytes = MIN_DISK_BYTES;
        assert!(validate_disk(&disk).is_ok());
    }
}
