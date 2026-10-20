//! Filesystem locations. Every path can be overridden with an environment variable,
//! which keeps tests isolated from the real installation.

use std::{os::unix::fs::MetadataExt, path::PathBuf};

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "jg-team-dev")
}

/// `$XDG_RUNTIME_DIR/jg-team-dev` (or `/tmp/jg-team-dev-<uid>`).
pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JGT_RUNTIME_DIR") {
        return PathBuf::from(dir);
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("jg-team-dev"),
        _ => {
            let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
            PathBuf::from(format!("/tmp/jg-team-dev-{uid}"))
        }
    }
}

/// The hub's unix socket.
pub fn socket_path() -> PathBuf {
    match std::env::var("JGT_SOCKET") {
        Ok(path) => PathBuf::from(path),
        Err(_) => runtime_dir().join("hub.sock"),
    }
}

/// `~/.local/share/jg-team-dev`
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JGT_DATA_DIR") {
        return PathBuf::from(dir);
    }
    project_dirs()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".jg-team-dev"))
}

/// `~/.config/jg-team-dev`
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JGT_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    project_dirs()
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".jg-team-dev"))
}

pub fn db_path() -> PathBuf {
    data_dir().join("jg-team-dev.db")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into())))
}
