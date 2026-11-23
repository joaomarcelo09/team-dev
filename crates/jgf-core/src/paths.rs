//! Filesystem locations of Dark Factory. `JGF_DATA_DIR` overrides the data directory,
//! which keeps tests isolated from a real installation.

use std::path::PathBuf;

/// Name of the Dark Factory state directory (never `.team`).
pub const STATE_DIR_NAME: &str = "jg-factory";

/// `~/.local/share/jg-factory`
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("JGF_DATA_DIR") {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("", "", STATE_DIR_NAME)
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from(format!(".{STATE_DIR_NAME}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_is_not_the_stable_state() {
        let dir = data_dir();
        let text = dir.to_string_lossy();
        assert!(text.contains(STATE_DIR_NAME));
        assert!(!text.contains("jg-team-dev"));
        assert!(!text.ends_with(".team"));
    }
}
