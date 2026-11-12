//! `jg-team-dev settings`: the Claude Code settings and MCP config that sessions started by
//! jg-team-dev load with `--settings` and `--mcp-config`. Nothing is written to
//! `~/.claude/settings.json`, so other tools' hooks (Liaison, …) never see these sessions.

use crate::hooks::HookEvent;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Name of the MCP server in Claude Code; its tools appear as `mcp__jg-team-dev__chat_*`.
pub const MCP_NAME: &str = "jg-team-dev";

fn exe() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the jg-team-dev executable")?;
    Ok(exe.canonicalize().unwrap_or(exe))
}

fn quote(path: &Path) -> String {
    let s = path.to_string_lossy();
    if s.contains([' ', '\'', '"']) { format!("'{}'", s.replace('\'', r"'\''")) } else { s.into_owned() }
}

fn hook_timeout(event: HookEvent) -> u64 {
    match event {
        HookEvent::Stop => 3600,
        HookEvent::PermissionRequest => 900,
        HookEvent::SessionEnd => 5,
        _ => 15,
    }
}

/// Hooks for every event, plus a rule that lets agents message the user without a prompt.
#[allow(dead_code)] // launches use `claude_settings_with_rtk`
pub fn claude_settings(exe: &Path) -> Value {
    claude_settings_for_role(exe, None)
}

/// Claude Code settings with an optional role passed to every hook invocation (`jg-team-dev hook --role <role> <event>`).
#[allow(dead_code)] // launches use `claude_settings_with_rtk`
pub fn claude_settings_for_role(exe: &Path, role: Option<&str>) -> Value {
    claude_settings_with_rtk(exe, role, None)
}

/// The `rtk` program, from `[commands] rtk` or the usual places; `None` when it is not installed.
pub fn find_rtk() -> Option<PathBuf> {
    let config = jgt_core::config::Config::load();
    jgt_core::launch::find_program("rtk", config.commands.rtk.as_deref()).ok()
}

/// `claude_settings_for_role`, plus rtk's Bash `PreToolUse` hook (`<rtk> hook claude`) when `rtk` is given.
pub fn claude_settings_with_rtk(exe: &Path, role: Option<&str>, rtk: Option<&Path>) -> Value {
    let mut hooks = serde_json::Map::new();
    let role_flag = role.map(|r| format!(" --role {r}")).unwrap_or_default();
    for event in HookEvent::ALL {
        hooks.insert(
            event.claude_name().into(),
            json!([{
                "hooks": [{
                    "type": "command",
                    "command": format!("{} hook{role_flag} {}", quote(exe), event.arg()),
                    "timeout": hook_timeout(event),
                }]
            }]),
        );
    }
    if let Some(rtk) = rtk {
        hooks.insert(
            "PreToolUse".into(),
            json!([{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": format!("{} hook claude", quote(rtk)) }]
            }]),
        );
    }
    json!({
        "hooks": hooks,
        "permissions": { "allow": [format!("mcp__{MCP_NAME}")] },
    })
}

/// The MCP server config for `--mcp-config` (used together with `--strict-mcp-config`).
pub fn mcp_config(exe: &Path) -> Value {
    json!({
        "mcpServers": {
            MCP_NAME: {
                "type": "stdio",
                "command": exe.to_string_lossy(),
                "args": ["mcp", "--harness", "claude-code"],
            }
        }
    })
}

pub fn print(mcp: bool) -> Result<()> {
    let exe = exe()?;
    let value = if mcp { mcp_config(&exe) } else { claude_settings_with_rtk(&exe, None, find_rtk().as_deref()) };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_cover_every_hook_event() {
        let s = claude_settings(Path::new("/usr/bin/jg-team-dev"));
        for event in HookEvent::ALL {
            let command = s["hooks"][event.claude_name()][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(command, format!("/usr/bin/jg-team-dev hook {}", event.arg()));
        }
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["timeout"], 3600);
        assert_eq!(s["permissions"]["allow"][0], "mcp__jg-team-dev");
    }

    #[test]
    fn settings_for_role_passes_role_flag() {
        let s = claude_settings_for_role(Path::new("/usr/bin/jg-team-dev"), Some("coordinator"));
        for event in HookEvent::ALL {
            let command = s["hooks"][event.claude_name()][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(command, format!("/usr/bin/jg-team-dev hook --role coordinator {}", event.arg()));
        }
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let s = claude_settings(Path::new("/opt/my tools/jg-team-dev"));
        let command = s["hooks"]["Stop"][0]["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(command, "'/opt/my tools/jg-team-dev' hook stop");
    }

    #[test]
    fn rtk_adds_a_bash_pre_tool_use_hook() {
        let exe = Path::new("/usr/bin/jg-team-dev");
        for role in [None, Some("coordinator")] {
            let s = claude_settings_with_rtk(exe, role, Some(Path::new("/path/rtk")));
            let group = &s["hooks"]["PreToolUse"][0];
            assert_eq!(group["matcher"], "Bash");
            assert_eq!(group["hooks"][0]["type"], "command");
            assert_eq!(group["hooks"][0]["command"], "/path/rtk hook claude");
            assert!(group["hooks"][0].get("timeout").is_none());
            assert_eq!(s["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
            let without = claude_settings_for_role(exe, role);
            for event in HookEvent::ALL {
                assert_eq!(s["hooks"][event.claude_name()], without["hooks"][event.claude_name()]);
            }
            assert_eq!(s["hooks"].as_object().unwrap().len(), HookEvent::ALL.len() + 1);
            assert_eq!(s["permissions"], without["permissions"]);
        }
    }

    #[test]
    fn rtk_path_with_spaces_is_quoted() {
        let s = claude_settings_with_rtk(Path::new("/usr/bin/jg-team-dev"), None, Some(Path::new("/opt/my tools/rtk")));
        assert_eq!(s["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "'/opt/my tools/rtk' hook claude");
    }

    #[test]
    fn no_rtk_means_no_pre_tool_use() {
        let exe = Path::new("/usr/bin/jg-team-dev");
        for role in [None, Some("coordinator")] {
            let s = claude_settings_with_rtk(exe, role, None);
            assert!(s["hooks"].get("PreToolUse").is_none());
            assert_eq!(s, claude_settings_for_role(exe, role));
        }
    }

    #[test]
    fn mcp_config_runs_the_mcp_subcommand() {
        let m = mcp_config(Path::new("/usr/bin/jg-team-dev"));
        assert_eq!(m["mcpServers"]["jg-team-dev"]["command"], "/usr/bin/jg-team-dev");
        assert_eq!(m["mcpServers"]["jg-team-dev"]["args"][0], "mcp");
    }
}
