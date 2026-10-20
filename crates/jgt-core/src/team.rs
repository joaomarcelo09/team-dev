//! What team sessions are started with. Shared by `jg-team-dev start` and the reconnect launcher,
//! so a Coordinator always comes back with the same hooks, MCP server and skills.

use crate::{config::Config, model::COORDINATOR, paths};
use std::path::{Path, PathBuf};

/// Plugin name; its skills are invoked as `/jg-team-dev:<skill>`.
pub const PLUGIN_NAME: &str = "jg-team-dev";

/// Set on the sessions jg-team-dev launches; their hooks inherit it.
pub const ROLE_ENV: &str = "JGT_ROLE";

/// Where `settings.json`, `mcp.json` and `plugin/` for team sessions are written.
pub fn files_dir() -> PathBuf {
    paths::data_dir().join("claude")
}

/// Claude Code flags for a Coordinator in `folder`, without `--bg` and the prompt.
pub fn coordinator_args(files: &Path, folder: &Path, config: &Config) -> Vec<String> {
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    let file = |n: &str| files.join(n).to_string_lossy().into_owned();
    vec![
        "--model".into(),
        config.team.model.clone(),
        "--effort".into(),
        config.team.effort.clone(),
        "--permission-mode".into(),
        config.team.permission_mode.clone(),
        "--settings".into(),
        file("settings.json"),
        "--mcp-config".into(),
        file("mcp.json"),
        "--strict-mcp-config".into(),
        // Leave out user settings so other tools' global hooks (Liaison, …) never see this session.
        "--setting-sources".into(),
        "project,local".into(),
        "--plugin-dir".into(),
        file("plugin"),
        "-n".into(),
        format!("{COORDINATOR} · {name}"),
    ]
}

/// Whether Claude Code still keeps the background session's saved options. A plain
/// `claude --bg --resume <id>` restores them; passing flags then would start a copy under a new
/// id. Once the record is gone (`claude rm`), the flags must be passed again.
///
/// The record lives in `~/.claude/jobs/<short id>/state.json`, an undocumented Claude Code file:
/// if its layout changes this returns false, and the session still resumes, with our flags.
pub fn has_saved_options(external_id: &str) -> bool {
    let short = crate::model::short_id(external_id);
    paths::home_dir().join(".claude").join("jobs").join(short).join("state.json").is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_args_isolate_the_session() {
        let args = coordinator_args(Path::new("/data/claude"), Path::new("/work/app"), &Config::default()).join(" ");
        assert!(args.starts_with("--model opus --effort medium --permission-mode auto"));
        assert!(args.contains("--settings /data/claude/settings.json --mcp-config /data/claude/mcp.json --strict-mcp-config"));
        assert!(args.contains("--setting-sources project,local --plugin-dir /data/claude/plugin"));
        assert!(args.ends_with("-n coordinator · app"));
    }
}
