//! What team sessions are started with. Shared by `jg-team-dev start` and the reconnect launcher,
//! so a Coordinator always comes back with the same hooks, MCP server and skills.

use crate::{config::Config, model::COORDINATOR, paths};
use std::path::{Path, PathBuf};

/// Plugin name; its skills are invoked as `/jg-team-dev:<skill>`.
pub const PLUGIN_NAME: &str = "jg-team-dev";

/// Set on the sessions jg-team-dev launches; their hooks inherit it.
pub const ROLE_ENV: &str = "JGT_ROLE";

/// Hub id of the Coordinator, set on the workers it starts.
pub const PARENT_ENV: &str = "JGT_PARENT";

/// The hub's `external_id` of a worker whose harness does not register itself; our MCP server
/// reads it to find the session (PRD Part B).
pub const SESSION_ENV: &str = "JGT_SESSION";

/// The board task (hub id) a worker works on, set by `jg-team-dev worker --task-id`; `task_get`
/// reads it by default.
pub const TASK_ENV: &str = "JGT_TASK";

/// Worker skills a Coordinator can start (`jg-team-dev worker <skill>`).
pub const WORKER_SKILLS: &[&str] = &["coder", "reviewer", "git-coder"];

/// The worker skill allowed to change git state: it alone commits, pushes, opens and merges the
/// pull request (and so runs `gh`).
pub const GIT_CODER: &str = "git-coder";

/// The git write steps only the git-coder may run.
pub const GIT_WRITE_SUBCOMMANDS: &[&str] = &["commit", "push", "merge", "rebase", "reset --hard"];

/// The bash commands only the git-coder may run: the git write steps and the `gh` CLI. OpenCode's
/// permission rules take them as globs; Claude Code takes them through [`git_write_tools`], and
/// Antigravity through [`git_permission_settings`].
///
/// The `-C` and `-c` forms are the ones the team's worktree workflow uses (`git -C <worktree>
/// commit`), and `--no-pager` is a common option before the subcommand. A wildcard covers the
/// options after `-C`/`-c`, so `git -C * commit*` also matches `git -C <path> -c k=v commit`.
pub const GIT_WRITE_GLOBS: &[&str] = &[
    "git commit*",
    "git -C * commit*",
    "git -c * commit*",
    "git --no-pager commit*",
    "git --no-pager -C * commit*",
    "git --no-pager -c * commit*",
    "git push*",
    "git -C * push*",
    "git -c * push*",
    "git --no-pager push*",
    "git --no-pager -C * push*",
    "git --no-pager -c * push*",
    "git merge*",
    "git -C * merge*",
    "git -c * merge*",
    "git --no-pager merge*",
    "git --no-pager -C * merge*",
    "git --no-pager -c * merge*",
    "git rebase*",
    "git -C * rebase*",
    "git -c * rebase*",
    "git --no-pager rebase*",
    "git --no-pager -C * rebase*",
    "git --no-pager -c * rebase*",
    "git reset --hard*",
    "git -C * reset --hard*",
    "git -c * reset --hard*",
    "git --no-pager reset --hard*",
    "git --no-pager -C * reset --hard*",
    "git --no-pager -c * reset --hard*",
    "gh*",
];

/// Whether `skill` may run [`GIT_WRITE_GLOBS`]: only the git-coder.
pub fn may_run_git(skill: &str) -> bool {
    skill == GIT_CODER
}

/// [`GIT_WRITE_GLOBS`] as Claude Code Bash rules (`Bash(git commit*)`, `Bash(git -C * commit*)`).
///
/// Claude Code matches a rule with a `*` as a wildcard. A trailing `:*` is its prefix syntax, but
/// when the same rule also holds a `*` it is matched literally (checked in Claude Code's rule
/// validator), so the wildcard form is the one that covers the `-C`/`-c` patterns.
pub fn git_write_tools() -> Vec<String> {
    GIT_WRITE_GLOBS.iter().map(|glob| format!("Bash({glob})")).collect()
}

/// `--allowedTools`/`--disallowedTools` flags for a role's git policy: allowed for the git-coder,
/// denied for every other role (the Coordinator included).
pub fn git_permission_args(skill: &str) -> Vec<String> {
    let flag = if may_run_git(skill) { "--allowedTools" } else { "--disallowedTools" };
    git_write_tools().into_iter().flat_map(|tool| [flag.to_string(), tool]).collect()
}

/// Claude Code built-in tools no team role uses. Each one still costs its schema in every turn
/// (`Artifact` alone is ~15k tokens), so they are denied at launch. The Board, chat and git flow
/// run through the hub and the git-coder instead (see the context audit, `.team/reports`).
pub const UNUSED_CLAUDE_TOOLS: &[&str] =
    &["Artifact", "AskUserQuestion", "Agent", "ScheduleWakeup", "ReportFindings", "Skill"];

/// Tools only the Coordinator keeps: Claude Code's background isolation asks it to call
/// `EnterWorktree`, while workers always start inside their task worktree.
const WORKER_ONLY_DENIED_TOOLS: &[&str] = &["EnterWorktree", "ExitWorktree"];

/// jg-team-dev MCP tools only the Coordinator uses (the Board, inbox and chat title). Workers keep
/// `chat_status`, `chat_send`, `chat_notify`, `task_get` and `chat_ask` (a blocked worker may ask).
const WORKER_DENIED_MCP_TOOLS: &[&str] = &[
    "mcp__jg-team-dev__chat_inbox",
    "mcp__jg-team-dev__chat_wait",
    "mcp__jg-team-dev__chat_set_title",
    "mcp__jg-team-dev__task_list",
    "mcp__jg-team-dev__task_update",
    "mcp__jg-team-dev__task_move",
    "mcp__jg-team-dev__task_create",
    "mcp__jg-team-dev__task_log",
];

/// `--disallowedTools` flags that drop the tools `skill` never uses from its prompt.
pub fn unused_tool_args(skill: &str) -> Vec<String> {
    let worker_only: Vec<&str> = if skill == COORDINATOR {
        Vec::new()
    } else {
        WORKER_ONLY_DENIED_TOOLS.iter().chain(WORKER_DENIED_MCP_TOOLS).copied().collect()
    };
    UNUSED_CLAUDE_TOOLS
        .iter()
        .chain(&worker_only)
        .flat_map(|tool| ["--disallowedTools".to_string(), (*tool).to_string()])
        .collect()
}

/// Antigravity's session settings that deny the git write steps and `gh` to a non-git-coder role,
/// or `None` for the git-coder. Antigravity matches `command(...)` as a command prefix, so these
/// cover `git commit -m …` but not options before the subcommand (`git -C <path> commit`): denying
/// `command(git -C)` would also deny read-only `git -C <path> status`, so that form stays prompt-
/// enforced (see README).
pub fn git_permission_settings(skill: &str) -> Option<String> {
    if may_run_git(skill) {
        return None;
    }
    let mut deny: Vec<String> =
        GIT_WRITE_SUBCOMMANDS.iter().map(|sub| format!("command(git {sub})")).collect();
    deny.push("command(gh)".into());
    Some(serde_json::json!({ "permissions": { "deny": deny } }).to_string())
}

/// Where `settings.json`, `mcp.json` and `plugin/` for team sessions are written.
pub fn files_dir() -> PathBuf {
    paths::data_dir().join("claude")
}

/// Claude Code flags for a Coordinator in `folder`, without `--bg` and the prompt. The Coordinator
/// gets the git write commands denied like every other non-git-coder role.
pub fn coordinator_args(files: &Path, folder: &Path, config: &Config) -> Vec<String> {
    let name = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
    let mut args =
        session_args_for_role(files, &format!("{COORDINATOR} · {name}"), &config.team.for_skill(COORDINATOR, folder), Some(COORDINATOR));
    args.extend(git_permission_args(COORDINATOR));
    args.extend(unused_tool_args(COORDINATOR));
    args
}

/// Chat title of a worker: `<skill> · <task>`.
pub fn worker_title(skill: &str, task: &str) -> String {
    format!("{skill} · {}", task.trim())
}

pub use crate::harness::{session_args, session_args_for_role};

/// The `--settings` file for a session, optionally role-specific: `settings.json` (or `settings-<role>.json`),
/// or for a compact window a copy of it with `autoCompactWindow` set (`settings[-<role>]-compact-<window>.json`).
/// The window is in the file name because a background session respawns with the flags it was started with:
/// the path must keep meaning the same window. The copy is rewritten from the base file on each call, so it
/// follows upgrades; if that fails, the session runs with the base file and Claude Code's own window.
pub fn settings_file_for_role(files: &Path, compact_window: Option<u64>, role: Option<&str>) -> PathBuf {
    let base_name = match role {
        Some(r) => format!("settings-{r}.json"),
        None => "settings.json".to_string(),
    };
    let base = files.join(&base_name);
    let fallback = files.join("settings.json");
    let base_to_read = if base.is_file() { &base } else { &fallback };
    let Some(window) = compact_window else { return base };
    let path_name = match role {
        Some(r) => format!("settings-{r}-compact-{window}.json"),
        None => format!("settings-compact-{window}.json"),
    };
    let path = files.join(path_name);
    let derived = std::fs::read_to_string(base_to_read)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|mut v| {
            v.as_object_mut()?.insert("autoCompactWindow".into(), window.into());
            serde_json::to_string_pretty(&v).ok()
        });
    match derived {
        Some(text) if std::fs::write(&path, &text).is_ok() => path,
        _ if path.is_file() => path,
        _ => base,
    }
}

/// The `--settings` file for a session without a specific role: `settings.json` (or `settings-compact-<window>.json`).
pub fn settings_file(files: &Path, compact_window: Option<u64>) -> PathBuf {
    settings_file_for_role(files, compact_window, None)
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

/// Queues `text` for the session `to` (a worker reporting to its Coordinator) and wakes `to`
/// when it is a sleeping Coordinator, so the message is read right away.
pub async fn relay(client: &crate::HubClient, to: &str, from: Option<&str>, text: &str) -> Result<(), String> {
    use crate::api::RelayRequest;

    // A result for a Coordinator that has since reset goes to the folder's current Coordinator.
    let mut to = to.to_string();
    let first: crate::Session = client.get(&format!("/v1/sessions/{to}")).await.map_err(|e| e.to_string())?;
    if first.is_retired()
        && let Some(cwd) = first.cwd.as_deref()
        && let Some(current) = current_coordinator(client, cwd).await?
    {
        to = current.id;
    }
    let to = to.as_str();
    let _: crate::Message = client
        .post(
            &format!("/v1/sessions/{to}/relay"),
            &RelayRequest { from_session: from.map(str::to_owned), text: text.to_string() },
        )
        .await
        .map_err(|e| e.to_string())?;
    let target: crate::Session = client.get(&format!("/v1/sessions/{to}")).await.map_err(|e| e.to_string())?;
    wake(client, &target).await
}

/// Normalizes a folder path for comparison: trims whitespace and trailing slashes,
/// keeping "/" as "/".
pub fn normalize_folder(folder: &str) -> &str {
    let trimmed = folder.trim();
    match trimmed.trim_end_matches('/') {
        "" if trimmed.starts_with('/') => "/",
        rest => rest,
    }
}

/// Returns the root project folder for `path`: normalizes the folder, then if the path contains
/// `/.team/worktrees/` (or ends with `/.team/worktrees`), returns the part before `/.team/worktrees`;
/// if that part is empty returns "/".
///
/// Note that a team deliberately started inside a worktree folder maps to the outer project (first match)
/// and is refused while that outer project's Coordinator is live.
pub fn project_folder(path: &str) -> &str {
    let norm = normalize_folder(path);
    const WORKTREES: &str = "/.team/worktrees";
    for (idx, _) in norm.match_indices(WORKTREES) {
        let rest = &norm[idx + WORKTREES.len()..];
        if rest.is_empty() || rest.starts_with('/') {
            let prefix = &norm[..idx];
            return if prefix.is_empty() { "/" } else { prefix };
        }
    }
    norm
}

/// The Coordinator of `folder` now: the newest coordinator session of that folder not ended.
pub async fn current_coordinator(client: &crate::HubClient, folder: &str) -> Result<Option<crate::Session>, String> {
    let all: crate::api::SessionsResponse =
        client.get("/v1/sessions?archived=true").await.map_err(|e| e.to_string())?;
    let folder = project_folder(folder);
    Ok(all
        .sessions
        .into_iter()
        .filter(|s| {
            s.is_coordinator()
                && s.status != crate::SessionStatus::Ended
                && s.cwd.as_deref().is_some_and(|c| project_folder(c) == folder)
        })
        .max_by_key(|s| s.created_at))
}

/// Wakes `session` when it is a sleeping (disconnected) Coordinator, so it reads the messages
/// queued for it right away. The hub never starts agents: the client that queued them does.
pub async fn wake(client: &crate::HubClient, session: &crate::Session) -> Result<(), String> {
    if session.is_coordinator() && session.status == crate::SessionStatus::Disconnected {
        crate::launch::reconnect(client, session, crate::ReconnectMode::Background).await?;
    }
    Ok(())
}

/// The workspace folder for `session`, if it belongs to a team.
pub fn session_workspace(session: &crate::Session, sessions: &[crate::Session]) -> Option<String> {
    if session.is_coordinator() {
        return session
            .cwd
            .as_deref()
            .map(project_folder)
            .filter(|k| !k.is_empty())
            .map(str::to_string);
    }
    if session.is_worker() {
        if let Some(parent_id) = session.parent_id.as_deref()
            && let Some(parent) = sessions.iter().find(|p| p.id == parent_id && p.is_coordinator())
        {
            return parent
                .cwd
                .as_deref()
                .map(project_folder)
                .filter(|k| !k.is_empty())
                .map(str::to_string);
        }
        if let Some(folder) = session
            .cwd
            .as_deref()
            .map(project_folder)
            .filter(|k| !k.is_empty())
            && coordinator_for(sessions, folder).is_some()
        {
            return Some(folder.to_string());
        }
    }
    None
}

/// The Coordinator for `ws`: the newest live Coordinator of that folder.
pub fn coordinator_for<'a>(sessions: &'a [crate::Session], ws: &str) -> Option<&'a crate::Session> {
    let ws_norm = project_folder(ws);
    if ws_norm.is_empty() {
        return None;
    }
    let mut coords: Vec<&'a crate::Session> = sessions
        .iter()
        .filter(|s| {
            s.is_coordinator()
                && s.cwd.as_deref().map(project_folder) == Some(ws_norm)
        })
        .collect();
    if coords.is_empty() {
        return None;
    }
    coords.sort_by_key(|s| {
        (
            s.status != crate::SessionStatus::Ended,
            !s.archived,
            s.last_message_at.unwrap_or(s.updated_at),
            s.created_at,
        )
    });
    coords.pop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinator_args_isolate_the_session() {
        let args = coordinator_args(Path::new("/data/claude"), Path::new("/work/app"), &Config::default()).join(" ");
        assert!(args.starts_with("--model opus --effort medium --permission-mode auto"));
        assert!(args.contains("--settings /data/claude/settings-coordinator.json --mcp-config /data/claude/mcp.json --strict-mcp-config"));
        assert!(args.contains("--setting-sources project,local --plugin-dir /data/claude/plugin"));
        assert!(args.contains("-n coordinator · app"));
        // The Coordinator is denied the git write steps and gh like every non-git-coder role.
        for tool in git_write_tools() {
            assert!(args.contains(&format!("--disallowedTools {tool}")), "{tool}: {args}");
            assert!(!args.contains(&format!("--allowedTools {tool}")), "{tool}: {args}");
        }
        // Read-only git and `git worktree add` stay allowed: no rule denies them.
        assert!(!args.contains("--disallowedTools Bash(git worktree"), "{args}");
        assert!(!args.contains("--disallowedTools Bash(git status"), "{args}");
    }

    #[test]
    fn a_compact_window_gets_its_own_settings_file() {
        let dir = std::env::temp_dir().join(format!("jgt-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(settings_file(&dir, Some(120_000)), dir.join("settings.json"), "no base file to copy");
        std::fs::write(dir.join("settings.json"), r#"{"hooks": {"Stop": []}}"#).unwrap();
        assert_eq!(settings_file(&dir, None), dir.join("settings.json"));
        let path = settings_file(&dir, Some(120_000));
        assert_eq!(path, dir.join("settings-compact-120000.json"));
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["autoCompactWindow"], 120_000);
        assert!(v["hooks"]["Stop"].is_array());

        // Role-specific settings file (e.g. settings-coordinator-compact-120000.json).
        std::fs::write(dir.join("settings-coordinator.json"), r#"{"hooks": {"Stop": []}}"#).unwrap();
        assert_eq!(settings_file_for_role(&dir, None, Some(COORDINATOR)), dir.join("settings-coordinator.json"));
        let coord_path = settings_file_for_role(&dir, Some(120_000), Some(COORDINATOR));
        assert_eq!(coord_path, dir.join("settings-coordinator-compact-120000.json"));
        let coord_v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&coord_path).unwrap()).unwrap();
        assert_eq!(coord_v["autoCompactWindow"], 120_000);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_lists_become_one_flag_per_tool() {
        let mut settings = Config::default().team.for_skill("reviewer", Path::new("/nonexistent"));
        settings.allowed_tools = vec!["Bash(cargo test:*)".into(), "Read".into()];
        settings.disallowed_tools = vec!["WebFetch".into()];
        let args = session_args(Path::new("/f"), "t", &settings);
        let tail = &args[args.len() - 6..];
        assert_eq!(tail, ["--allowedTools", "Bash(cargo test:*)", "--allowedTools", "Read", "--disallowedTools", "WebFetch"]);
    }

    #[test]
    fn unused_tools_are_denied_and_only_the_coordinator_keeps_enter_worktree() {
        let coder = unused_tool_args("coder");
        assert!(coder.chunks(2).all(|c| c[0] == "--disallowedTools"));
        for tool in ["Artifact", "Agent", "Skill", "EnterWorktree"] {
            assert!(coder.contains(&tool.to_string()), "{tool}");
        }
        let coordinator = unused_tool_args(COORDINATOR);
        assert!(coordinator.contains(&"Artifact".to_string()));
        assert!(!coordinator.contains(&"EnterWorktree".to_string()));
    }

    #[test]
    fn workers_lose_the_coordinator_only_mcp_tools() {
        let coder = unused_tool_args("coder");
        for tool in ["task_update", "task_move", "task_list", "chat_wait", "chat_inbox"] {
            assert!(coder.contains(&format!("mcp__jg-team-dev__{tool}")), "{tool}");
        }
        for tool in ["chat_status", "chat_send", "chat_notify", "chat_ask", "task_get"] {
            assert!(!coder.contains(&format!("mcp__jg-team-dev__{tool}")), "{tool}");
        }
        assert!(!unused_tool_args(COORDINATOR).iter().any(|t| t.starts_with("mcp__jg-team-dev__")));
    }

    #[test]
    fn only_the_git_coder_may_run_git_writes_and_gh() {
        assert!(may_run_git(GIT_CODER));
        for skill in ["coder", "reviewer", "coordinator", ""] {
            assert!(!may_run_git(skill), "{skill}");
        }
        assert!(GIT_WRITE_GLOBS.contains(&"git commit*") && GIT_WRITE_GLOBS.contains(&"gh*"));
        let tools = git_write_tools();
        assert!(tools.contains(&"Bash(git commit*)".to_string()));
        assert!(tools.contains(&"Bash(gh*)".to_string()));
        // No `:*` prefix rules: Claude Code reads them literally when the rule holds a wildcard.
        assert!(tools.iter().all(|tool| !tool.contains(":*")), "{tools:?}");
        let denied = git_permission_args("coder").join(" ");
        assert!(denied.contains("--disallowedTools Bash(git -C * commit*)"), "{denied}");
        let allowed = git_permission_args(GIT_CODER).join(" ");
        assert!(allowed.contains("--allowedTools Bash(git -C * commit*)"), "{allowed}");
        assert!(!allowed.contains("--disallowedTools"), "{allowed}");
    }

    #[test]
    fn git_write_rules_cover_options_before_the_subcommand() {
        for sub in GIT_WRITE_SUBCOMMANDS {
            for glob in [
                format!("git {sub}*"),
                format!("git -C * {sub}*"),
                format!("git -c * {sub}*"),
                format!("git --no-pager {sub}*"),
                format!("git --no-pager -C * {sub}*"),
                format!("git --no-pager -c * {sub}*"),
            ] {
                assert!(GIT_WRITE_GLOBS.contains(&glob.as_str()), "{glob} missing");
            }
        }
        // Read-only git is not denied.
        for glob in GIT_WRITE_GLOBS {
            assert!(!glob.contains("worktree") && !glob.contains("status") && !glob.contains("log"), "{glob}");
        }
    }

    #[test]
    fn antigravity_denies_only_plain_git_write_commands() {
        assert!(git_permission_settings(GIT_CODER).is_none());
        let settings: serde_json::Value = serde_json::from_str(&git_permission_settings("coder").unwrap()).unwrap();
        let deny: Vec<&str> =
            settings["permissions"]["deny"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        for sub in GIT_WRITE_SUBCOMMANDS {
            assert!(deny.contains(&format!("command(git {sub})").as_str()), "{deny:?}");
        }
        assert!(deny.contains(&"command(gh)"), "{deny:?}");
        // A blanket `command(git -C)` would also deny read-only `git -C <path> status`.
        assert!(!deny.iter().any(|rule| rule.starts_with("command(git -C")), "{deny:?}");
    }

    #[test]
    fn normalize_folder_cases() {
        assert_eq!(normalize_folder("/foo/bar/"), "/foo/bar");
        assert_eq!(normalize_folder("/foo/bar///"), "/foo/bar");
        assert_eq!(normalize_folder("/"), "/");
        assert_eq!(normalize_folder("///"), "/");
        assert_eq!(normalize_folder("/foo/bar"), "/foo/bar");
        assert_eq!(normalize_folder("foo/bar/"), "foo/bar");
        assert_eq!(normalize_folder("foo/bar"), "foo/bar");
    }

    #[test]
    fn project_folder_cases() {
        // trailing slash
        assert_eq!(project_folder("/w/.team/worktrees/jgt-5/"), "/w");
        assert_eq!(project_folder("/w/.team/worktrees/"), "/w");
        assert_eq!(project_folder("/w/.team/worktrees"), "/w");

        // nested subpath
        assert_eq!(project_folder("/w/.team/worktrees/jgt-5/crates/x"), "/w");

        // no worktree
        assert_eq!(project_folder("/w/crates/x"), "/w/crates/x");
        assert_eq!(project_folder("/w"), "/w");

        // root /
        assert_eq!(project_folder("/"), "/");

        // non-matches
        assert_eq!(project_folder("/w/.team/worktrees-x"), "/w/.team/worktrees-x");
        assert_eq!(project_folder("/w2"), "/w2");
        assert_eq!(project_folder("/w/.team"), "/w/.team");

        // worktree at root (empty prefix before /.team/worktrees)
        assert_eq!(project_folder("/.team/worktrees"), "/");
        assert_eq!(project_folder("/.team/worktrees/"), "/");
        assert_eq!(project_folder("/.team/worktrees/jgt-5"), "/");
    }

    #[test]
    fn session_workspace_maps_coordinator_and_worker_worktree_to_project() {
        let coord: crate::Session = serde_json::from_value(serde_json::json!({
            "id": "c1",
            "harness": "claude-code",
            "external_id": "ext-c1",
            "status": "idle",
            "listen": false,
            "mirror": false,
            "archived": false,
            "awaiting_reply": false,
            "created_at": 10,
            "updated_at": 10,
            "last_seen_at": 10,
            "role": "coordinator",
            "cwd": "/w/.team/worktrees/jgt-5",
        }))
        .unwrap();

        let worker_with_parent: crate::Session = serde_json::from_value(serde_json::json!({
            "id": "w1",
            "harness": "claude-code",
            "external_id": "ext-w1",
            "status": "active",
            "listen": false,
            "mirror": false,
            "archived": false,
            "awaiting_reply": false,
            "created_at": 20,
            "updated_at": 20,
            "last_seen_at": 20,
            "role": "coder",
            "parent_id": "c1",
            "cwd": "/w/.team/worktrees/jgt-5",
        }))
        .unwrap();

        let worker_without_parent: crate::Session = serde_json::from_value(serde_json::json!({
            "id": "w2",
            "harness": "claude-code",
            "external_id": "ext-w2",
            "status": "active",
            "listen": false,
            "mirror": false,
            "archived": false,
            "awaiting_reply": false,
            "created_at": 30,
            "updated_at": 30,
            "last_seen_at": 30,
            "role": "coder",
            "cwd": "/w/.team/worktrees/jgt-5",
        }))
        .unwrap();

        let sessions = vec![coord.clone(), worker_with_parent.clone(), worker_without_parent.clone()];

        assert_eq!(session_workspace(&coord, &sessions), Some("/w".to_string()));
        assert_eq!(session_workspace(&worker_with_parent, &sessions), Some("/w".to_string()));
        assert_eq!(session_workspace(&worker_without_parent, &sessions), Some("/w".to_string()));
    }
}
