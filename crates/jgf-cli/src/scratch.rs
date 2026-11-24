//! The scratch home of a real coder or reviewer child.
//!
//! A [`ScratchHome`] is a private directory (mode 0700) beside the run's isolated work tree.
//! The jg-sandbox lab exposes separate `work/` and `scratch/` directories; the work tree is
//! writable except for its read-only `.git` directory, and system/runtime directories plus
//! Claude's single credentials file are read-only.
//! `/tmp` is a fresh tmpfs, and `/proc` and `/dev` are fresh views. Direct network is disabled;
//! Claude API traffic uses the host-side egress proxy and its allowlist.
//!
//! The host credentials file is only located and linked; this module never reads its contents.
//! A token refresh may replace the symlink with a regular file in the scratch directory.

use std::{
    env,
    ffi::OsString,
    fs, io,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
};

use thiserror::Error;

/// File that holds the Claude Code credentials inside the host `.claude` directory.
pub const CREDENTIALS_FILE: &str = ".credentials.json";
/// Directory name of the host Claude configuration inside the real home.
const HOST_CLAUDE_DIR: &str = ".claude";
/// Permission bits of every directory of the scratch home.
const DIR_MODE: u32 = 0o700;
/// Host variables the child may keep. Everything else is dropped.
const KEPT_ENV_KEYS: [&str; 5] = ["PATH", "LANG", "LC_ALL", "TERM", "SHELL"];

/// Why a scratch home could not be prepared.
#[derive(Debug, Error)]
pub enum ScratchError {
    /// The current process has no home directory.
    #[error("HOME is not set, so the real Claude credentials cannot be located")]
    NoHome,

    /// The host Claude credentials file does not exist.
    #[error("the real Claude credentials file was not found at {}", path.display())]
    MissingCredentials {
        /// Expected path of the credentials file.
        path: PathBuf,
    },

    /// A directory or link of the scratch home could not be created.
    #[error("could not prepare the scratch home at {}: {source}", path.display())]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

/// The private home of one run's coder and reviewer children.
#[derive(Debug, Clone)]
pub struct ScratchHome {
    root: PathBuf,
    host_home: PathBuf,
    credentials_file: PathBuf,
}

impl ScratchHome {
    /// Creates the scratch home under `root`, linking the host credentials of `real_home`.
    ///
    /// The host credentials file is only checked and linked, never opened.
    pub fn prepare(root: impl Into<PathBuf>, real_home: &Path) -> Result<Self, ScratchError> {
        let root = root.into();
        let real_home =
            fs::canonicalize(real_home).map_err(|source| io_error(real_home, source))?;
        let credentials = real_home.join(HOST_CLAUDE_DIR).join(CREDENTIALS_FILE);
        check_credentials(&credentials)?;

        // Every child gets a fresh scratch home, so nothing an earlier child wrote stays
        // behind; the only file in the Claude config dir is the credentials link.
        if fs::symlink_metadata(&root).is_ok() {
            fs::remove_dir_all(&root).map_err(|source| io_error(&root, source))?;
        }
        create_private_dir(&root)?;
        for dir in ["home", "config", "data", "cache", "gh", "claude"] {
            create_private_dir(&root.join(dir))?;
        }

        let link = root.join("claude").join(CREDENTIALS_FILE);
        if fs::symlink_metadata(&link).is_ok() {
            fs::remove_file(&link).map_err(|source| io_error(&link, source))?;
        }
        symlink(&credentials, &link).map_err(|source| io_error(&link, source))?;
        let root = fs::canonicalize(&root).map_err(|source| io_error(&root, source))?;
        Ok(Self {
            root,
            host_home: real_home,
            credentials_file: credentials,
        })
    }

    /// Root directory of the scratch home.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `HOME`
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// `XDG_CONFIG_HOME`
    pub fn config(&self) -> PathBuf {
        self.root.join("config")
    }

    /// `XDG_DATA_HOME`
    pub fn data(&self) -> PathBuf {
        self.root.join("data")
    }

    /// `XDG_CACHE_HOME`
    pub fn cache(&self) -> PathBuf {
        self.root.join("cache")
    }

    /// `GH_CONFIG_DIR`, always empty.
    pub fn gh_config(&self) -> PathBuf {
        self.root.join("gh")
    }

    /// `CLAUDE_CONFIG_DIR`, holding only the credentials symlink.
    pub fn claude_config(&self) -> PathBuf {
        self.root.join("claude")
    }

    /// Path of the credentials symlink of the scratch Claude config.
    pub fn credentials_link(&self) -> PathBuf {
        self.claude_config().join(CREDENTIALS_FILE)
    }

    /// Host credentials file linked into the scratch Claude config; contents are never read.
    pub fn credentials_file(&self) -> &Path {
        &self.credentials_file
    }

    /// Canonical real home, used only to refuse broad runtime mounts over host credentials.
    pub fn host_home(&self) -> &Path {
        &self.host_home
    }

    /// Exact allowlisted environment passed to the isolated Claude child.
    pub(crate) fn environment(&self) -> Vec<(OsString, OsString)> {
        let mut values = Vec::new();
        for key in KEPT_ENV_KEYS {
            if let Some(value) = env::var_os(key) {
                values.push((OsString::from(key), value));
            }
        }
        for (key, value) in [
            ("HOME", self.home().into_os_string()),
            ("XDG_CONFIG_HOME", self.config().into_os_string()),
            ("XDG_DATA_HOME", self.data().into_os_string()),
            ("XDG_CACHE_HOME", self.cache().into_os_string()),
            ("GH_CONFIG_DIR", self.gh_config().into_os_string()),
            ("CLAUDE_CONFIG_DIR", self.claude_config().into_os_string()),
            ("GIT_CONFIG_GLOBAL", OsString::from("/dev/null")),
            ("GIT_CONFIG_SYSTEM", OsString::from("/dev/null")),
            ("GIT_TERMINAL_PROMPT", OsString::from("0")),
            ("TMPDIR", OsString::from("/tmp")),
        ] {
            values.push((OsString::from(key), value));
        }
        values
    }

    /// Gives `command` only the scratch home and the innocuous host variables.
    pub fn apply(&self, command: &mut Command) {
        command.env_clear();
        command.envs(self.environment());
    }
}

/// The real home of the current process.
pub fn real_home() -> Result<PathBuf, ScratchError> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| !home.as_os_str().is_empty())
        .ok_or(ScratchError::NoHome)
}

fn check_credentials(path: &Path) -> Result<(), ScratchError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) | Err(_) => Err(ScratchError::MissingCredentials {
            path: path.to_path_buf(),
        }),
    }
}

fn create_private_dir(path: &Path) -> Result<(), ScratchError> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
        .map_err(|source| io_error(path, source))?;
    fs::set_permissions(path, fs::Permissions::from_mode(DIR_MODE))
        .map_err(|source| io_error(path, source))
}

fn io_error(path: impl Into<PathBuf>, source: io::Error) -> ScratchError {
    ScratchError::Io {
        path: path.into(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_home(root: &Path) -> PathBuf {
        let home = root.join("host-home");
        fs::create_dir_all(home.join(HOST_CLAUDE_DIR)).expect("host claude dir");
        home
    }

    #[test]
    fn the_scratch_home_links_only_the_credentials_file() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let host = host_home(root.path());
        fs::write(
            host.join(HOST_CLAUDE_DIR).join(CREDENTIALS_FILE),
            b"placeholder",
        )
        .expect("fake credentials");
        fs::write(host.join(HOST_CLAUDE_DIR).join("settings.json"), b"{}").expect("fake settings");

        let scratch =
            ScratchHome::prepare(root.path().join("scratch"), &host).expect("prepare scratch");

        assert!(
            fs::symlink_metadata(scratch.credentials_link())
                .expect("credentials link")
                .file_type()
                .is_symlink()
        );
        let target = fs::read_link(scratch.credentials_link()).expect("link target");
        assert_eq!(target, host.join(HOST_CLAUDE_DIR).join(CREDENTIALS_FILE));
        assert!(!scratch.claude_config().join("settings.json").exists());
        assert!(scratch.gh_config().is_dir());
        assert_eq!(
            fs::read_dir(scratch.gh_config()).expect("gh dir").count(),
            0
        );
        let mode = fs::metadata(scratch.root())
            .expect("scratch metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn a_missing_host_credentials_file_is_a_clear_error() {
        let root = tempfile::TempDir::new().expect("temp dir");
        let host = host_home(root.path());

        let error = ScratchHome::prepare(root.path().join("scratch"), &host)
            .expect_err("missing credentials");

        assert!(error.to_string().contains("credentials file was not found"));
    }
}
