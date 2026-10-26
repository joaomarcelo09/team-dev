//! `jg-team-dev start [folder]`: starts the folder's Coordinator, a background Claude Code session
//! that loads jg-team-dev's hooks, MCP server and skills for this session only.

use crate::settings;
use anyhow::{Context, Result, bail};
use jgt_core::{
    COORDINATOR, HubClient, RETIRED, ReconnectMode, Session, SessionStatus, procs, short_id,
    api::{EndRequest, HealthResponse, PatchSessionRequest, SessionsResponse},
    config::Config,
    launch::{self, LaunchPlan},
    paths,
    team::{self, PLUGIN_NAME, ROLE_ENV},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Skills bundled into the binary and written to the plugin folder at start.
const SKILLS: &[(&str, &str)] = &[
    ("coordinator", include_str!("../../../skills/coordinator/SKILL.md")),
    ("planner", include_str!("../../../skills/planner/SKILL.md")),
    ("coder", include_str!("../../../skills/coder/SKILL.md")),
    ("reviewer", include_str!("../../../skills/reviewer/SKILL.md")),
    ("tester", include_str!("../../../skills/tester/SKILL.md")),
    ("git-coder", include_str!("../../../skills/git-coder/SKILL.md")),
];

/// Writes the plugin (manifest + skills) into `dir`, replacing what an older version wrote.
pub fn write_plugin(dir: &Path) -> Result<()> {
    let manifest = dir.join(".claude-plugin");
    std::fs::create_dir_all(&manifest)?;
    let meta = json!({
        "name": PLUGIN_NAME,
        "version": env!("CARGO_PKG_VERSION"),
        "description": "jg-team-dev team roles: the Coordinator and its workers",
    });
    std::fs::write(manifest.join("plugin.json"), serde_json::to_string_pretty(&meta)?)?;
    for (name, text) in SKILLS {
        let skill = dir.join("skills").join(name);
        std::fs::create_dir_all(&skill)?;
        std::fs::write(skill.join("SKILL.md"), text)?;
    }
    Ok(())
}

/// Writes the settings, MCP config and plugin that team sessions load; returns their folder.
/// Rewritten on every launch, so an upgraded binary's hooks and skills take effect.
pub fn prepare_files() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the jg-team-dev executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let files = team::files_dir();
    std::fs::create_dir_all(&files)?;
    std::fs::write(files.join("settings.json"), serde_json::to_string_pretty(&settings::claude_settings(&exe))?)?;
    std::fs::write(files.join("mcp.json"), serde_json::to_string_pretty(&settings::mcp_config(&exe))?)?;
    write_plugin(&files.join("plugin"))?;
    Ok(files)
}

/// Marks `folder` trusted in Claude Code's `~/.claude.json`, which `claude --bg` requires.
/// Returns whether the file changed.
pub fn trust(claude_json: &Path, folder: &Path) -> Result<bool> {
    let mut root: Value = match std::fs::read_to_string(claude_json) {
        Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", claude_json.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(e) => return Err(e).with_context(|| format!("reading {}", claude_json.display())),
    };
    let key = folder.to_string_lossy().into_owned();
    let root_obj = root.as_object_mut().context("~/.claude.json is not a JSON object")?;
    let projects = root_obj.entry("projects").or_insert_with(|| json!({}));
    let project = projects
        .as_object_mut()
        .context("`projects` in ~/.claude.json is not an object")?
        .entry(key)
        .or_insert_with(|| json!({}));
    if project.get("hasTrustDialogAccepted").and_then(Value::as_bool) == Some(true) {
        return Ok(false);
    }
    project
        .as_object_mut()
        .context("a project entry in ~/.claude.json is not an object")?
        .insert("hasTrustDialogAccepted".into(), json!(true));

    // Write next to the file and rename, so Claude Code never reads a half-written file.
    let tmp = claude_json.with_extension(format!("json.jgt-{}", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
    if let Ok(meta) = std::fs::metadata(claude_json) {
        std::fs::set_permissions(&tmp, meta.permissions())?;
    }
    std::fs::rename(&tmp, claude_json)?;
    Ok(true)
}

/// The `claude --bg` command that starts a Coordinator in `folder`. `handover` follows the skill
/// in the first prompt when a fresh Coordinator replaces one that reset.
pub fn coordinator_plan(claude: PathBuf, folder: &Path, files: &Path, config: &Config, handover: Option<&str>) -> LaunchPlan {
    let prompt = match handover {
        Some(text) => format!("/{PLUGIN_NAME}:{COORDINATOR} {text}"),
        None => format!("/{PLUGIN_NAME}:{COORDINATOR}"),
    };
    let mut args = vec!["--bg".to_string(), prompt];
    args.extend(team::coordinator_args(files, folder, config));
    LaunchPlan {
        path_prepend: claude.parent().map(Path::to_path_buf),
        program: claude,
        args,
        cwd: Some(folder.to_path_buf()),
        wait: true,
        log: None,
        env: vec![(ROLE_ENV.into(), COORDINATOR.into())],
    }
}

/// The folder's Coordinator, if one was started before and its context still exists.
async fn existing(client: &HubClient, folder: &Path) -> Result<Option<Session>> {
    let all: SessionsResponse = client.get("/v1/sessions?archived=true").await?;
    let folder = folder.to_string_lossy();
    Ok(all
        .sessions
        .into_iter()
        .filter(|s| s.is_coordinator() && s.status != SessionStatus::Ended && s.cwd.as_deref() == Some(&*folder))
        .max_by_key(|s| s.last_seen_at))
}

pub async fn start(folder: Option<PathBuf>) -> Result<()> {
    let folder = folder.map(Ok).unwrap_or_else(std::env::current_dir)?;
    let folder = folder.canonicalize().with_context(|| format!("{} does not exist", folder.display()))?;
    if !folder.is_dir() {
        bail!("{} is not a folder", folder.display());
    }
    let client = HubClient::default();
    client
        .get::<HealthResponse>("/v1/health")
        .await
        .context("the hub must be running: systemctl --user enable --now jg-team-dev-hub")?;

    if let Some(session) = existing(&client, &folder).await? {
        if session.status.is_connected() {
            println!("The Coordinator for {} is already running ({}).", folder.display(), session.id);
        } else {
            let output = launch::reconnect(&client, &session, ReconnectMode::Background).await.map_err(anyhow::Error::msg)?;
            println!("Waking the Coordinator for {}: {output}", folder.display());
        }
        return Ok(());
    }

    if trust(&paths::home_dir().join(".claude.json"), &folder)? {
        println!("Marked {} as trusted in ~/.claude.json", folder.display());
    }
    let files = prepare_files()?;

    let config = Config::load();
    let claude = launch::find_program("claude", config.commands.claude.as_deref()).map_err(anyhow::Error::msg)?;
    let plan = coordinator_plan(claude, &folder, &files, &config, None);
    let output = tokio::task::spawn_blocking(move || launch::spawn(&plan)).await?.map_err(anyhow::Error::msg)?;
    println!("Started the Coordinator for {}: {output}", folder.display());
    Ok(())
}

/// The first prompt's tail for a Coordinator that takes over after a reset.
pub fn handover_note(summary: Option<&Path>) -> String {
    let summary = summary.map(|p| format!(" Its summary of the last topic is `{}`.", p.display())).unwrap_or_default();
    format!(
        "You take over from the previous Coordinator of this folder, which reset after a topic change.{summary} \
         Earlier task summaries are in `.team/tasks/`. Greet the user as the fresh Coordinator and say in one line \
         which summary you can pick up from."
    )
}

/// `jg-team-dev reset` (run by a Coordinator): a fresh Coordinator takes over the folder; this one
/// is retired (its chat archived and read-only) and its process stops shortly after.
pub async fn reset(summary: Option<PathBuf>) -> Result<()> {
    let client = HubClient::default();
    let pid = procs::harness_pid().context("run `jg-team-dev reset` from the Coordinator's session")?;
    let old: Session = client
        .get(&format!("/v1/resolve?pid={pid}"))
        .await
        .context("this Claude Code session is not a jg-team-dev Coordinator")?;
    if !old.is_coordinator() {
        bail!("only a Coordinator can reset");
    }
    let folder = PathBuf::from(old.cwd.clone().context("the Coordinator has no folder")?);
    let summary = match summary {
        Some(p) => {
            let p = if p.is_absolute() { p } else { folder.join(p) };
            if !p.is_file() {
                bail!("summary {} does not exist; write it first", p.display());
            }
            Some(p.strip_prefix(&folder).map(Path::to_path_buf).unwrap_or(p))
        }
        None => None,
    };

    // Start the fresh Coordinator first: if that fails, the current one stays in charge.
    let files = prepare_files()?;
    let config = Config::load();
    let claude = launch::find_program("claude", config.commands.claude.as_deref()).map_err(anyhow::Error::msg)?;
    let plan = coordinator_plan(claude.clone(), &folder, &files, &config, Some(&handover_note(summary.as_deref())));
    let output = tokio::task::spawn_blocking(move || launch::spawn(&plan)).await?.map_err(anyhow::Error::msg)?;

    let _: Session = client
        .post(
            &format!("/v1/sessions/{}/end", old.id),
            &EndRequest { reason: format!("{RETIRED}: a fresh Coordinator took over"), gone: true },
        )
        .await?;
    let _: Session = client
        .patch(&format!("/v1/sessions/{}", old.id), &PatchSessionRequest { archived: Some(true), ..Default::default() })
        .await?;
    // Stop this session once it has finished its turn, and drop Claude Code's record of it:
    // a retired Coordinator is never woken again.
    let mut stop = Command::new("sh");
    stop.args(["-c", "sleep 15; \"$0\" stop \"$1\"; sleep 3; exec \"$0\" rm \"$1\""])
        .arg(&claude)
        .arg(short_id(&old.external_id))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::os::unix::process::CommandExt::process_group(&mut stop, 0);
    stop.spawn()?;

    println!(
        "A fresh Coordinator is starting for {} ({}). This session is retired and stops in about 15 seconds: \
         end your turn now without further messages; the new Coordinator greets the user.",
        folder.display(),
        output.lines().next().unwrap_or_default()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jgt-team-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn trust_adds_the_folder_and_keeps_everything_else() {
        let dir = temp("trust");
        let file = dir.join(".claude.json");
        std::fs::write(&file, r#"{"numStartups": 7, "projects": {"/other": {"hasTrustDialogAccepted": true, "x": 1}}}"#).unwrap();
        assert!(trust(&file, Path::new("/work/app")).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["projects"]["/work/app"]["hasTrustDialogAccepted"], true);
        assert_eq!(v["projects"]["/other"]["x"], 1);
        assert_eq!(v["numStartups"], 7);
        assert!(!trust(&file, Path::new("/work/app")).unwrap(), "second call changes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trust_creates_a_missing_file() {
        let dir = temp("trust-new");
        let file = dir.join(".claude.json");
        assert!(trust(&file, Path::new("/work/app")).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(v["projects"]["/work/app"]["hasTrustDialogAccepted"], true);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plugin_holds_the_manifest_and_skills() {
        let dir = temp("plugin");
        write_plugin(&dir).unwrap();
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join(".claude-plugin/plugin.json")).unwrap()).unwrap();
        assert_eq!(meta["name"], PLUGIN_NAME);
        let skill = std::fs::read_to_string(dir.join("skills/coordinator/SKILL.md")).unwrap();
        assert!(skill.starts_with("---\nname: coordinator\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handover_points_at_the_summary() {
        let note = handover_note(Some(Path::new(".team/tasks/task-1-css.md")));
        assert!(note.contains("`.team/tasks/task-1-css.md`"));
        assert!(note.contains("Earlier task summaries are in `.team/tasks/`"));
        let config = Config::default();
        let plan = coordinator_plan(PathBuf::from("claude"), Path::new("/w"), Path::new("/d"), &config, Some("Hi."));
        assert_eq!(plan.args[1], "/jg-team-dev:coordinator Hi.");
    }

    #[test]
    fn coordinator_plan_isolates_the_session() {
        let config = Config::default();
        let plan =
            coordinator_plan(PathBuf::from("/usr/bin/claude"), Path::new("/work/app"), Path::new("/data/claude"), &config, None);
        let args = plan.args.join(" ");
        assert!(args.starts_with("--bg /jg-team-dev:coordinator "));
        assert!(args.contains("--model opus --effort medium --permission-mode auto"));
        assert!(args.contains("--settings /data/claude/settings.json --mcp-config /data/claude/mcp.json --strict-mcp-config"));
        assert!(args.contains("--setting-sources project,local --plugin-dir /data/claude/plugin"));
        assert!(args.ends_with("-n coordinator · app"));
        assert_eq!(plan.cwd.as_deref(), Some(Path::new("/work/app")));
        assert_eq!(plan.env, vec![("JGT_ROLE".to_string(), "coordinator".to_string())]);
    }
}
