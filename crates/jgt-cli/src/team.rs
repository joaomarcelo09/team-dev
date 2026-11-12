//! `jg-team-dev start [folder]`: starts the folder's Coordinator, a background Claude Code session
//! that loads jg-team-dev's hooks, MCP server and skills for this session only.

use crate::settings;
use anyhow::{Context, Result, bail};
use jgt_core::{
    COORDINATOR, Harness, HubClient, RETIRED, ReconnectMode, Session, SessionStatus, TeamAgents, procs, short_id,
    api::{EndRequest, HealthResponse, PatchSessionRequest, SessionsResponse},
    config::Config,
    launch::{self, LaunchPlan},
    paths,
    team::{self, PLUGIN_NAME, ROLE_ENV},
};
use serde_json::{Value, json};
use std::{
    os::unix::io::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub const COORDINATOR_POLL_INTERVAL: Duration = Duration::from_millis(500);
pub const COORDINATOR_POLL_TIMEOUT: Duration = Duration::from_secs(20);

static TMP_COUNTER: AtomicU64 = AtomicU64::new(1);

struct FileLock {
    file: std::fs::File,
}

impl FileLock {
    fn acquire(path: &Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("opening lockfile {}", path.display()))?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            bail!("failed to lock {}", path.display());
        }
        Ok(Self { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let count = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{count}", std::process::id()));
    let res = std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path));
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.with_context(|| format!("writing {}", path.display()))
}

/// Skills bundled into the binary and written to the plugin folder at start.
const SKILLS: &[(&str, &str)] = &[
    ("coordinator", include_str!("../../../skills/coordinator/SKILL.md")),
    ("coder", include_str!("../../../skills/coder/SKILL.md")),
    ("reviewer", include_str!("../../../skills/reviewer/SKILL.md")),
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
    write_atomic(&manifest.join("plugin.json"), &serde_json::to_string_pretty(&meta)?)?;
    for (name, text) in SKILLS {
        let skill = dir.join("skills").join(name);
        std::fs::create_dir_all(&skill)?;
        write_atomic(&skill.join("SKILL.md"), text)?;
    }
    Ok(())
}

/// Writes the settings, MCP config and plugin that team sessions load; returns their folder.
/// Rewritten on every launch, so an upgraded binary's hooks and skills take effect.
pub fn prepare_files() -> Result<PathBuf> {
    let files = team::files_dir();
    prepare_files_in(&files)?;
    Ok(files)
}

fn prepare_files_in(files: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("locating the jg-team-dev executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    let rtk = settings::find_rtk();
    std::fs::create_dir_all(files)?;
    write_atomic(&files.join("settings.json"), &serde_json::to_string_pretty(&settings::claude_settings_with_rtk(&exe, None, rtk.as_deref()))?)?;
    write_atomic(&files.join("settings-coordinator.json"), &serde_json::to_string_pretty(&settings::claude_settings_with_rtk(&exe, Some(jgt_core::model::COORDINATOR), rtk.as_deref()))?)?;
    write_atomic(&files.join("mcp.json"), &serde_json::to_string_pretty(&settings::mcp_config(&exe))?)?;
    write_plugin(&files.join("plugin"))?;
    Ok(())
}

/// `prepare_files`, then what a worker on `harness` loads besides (OpenCode's agent files, in
/// our own config folder under `files`); returns the folder.
pub fn prepare_worker_files(harness: Harness) -> Result<PathBuf> {
    let files = team::files_dir();
    prepare_worker_files_in(&files, harness)?;
    Ok(files)
}

fn prepare_worker_files_in(files: &Path, harness: Harness) -> Result<()> {
    prepare_files_in(files)?;
    let adapter = jgt_core::harness::adapter(harness).with_context(|| format!("{} cannot run workers", harness.label()))?;
    adapter.install(files).with_context(|| format!("writing the {} worker files in {}", harness.label(), files.display()))
}

/// Marks `folder` trusted in Claude Code's `~/.claude.json`, which `claude --bg` requires.
/// Returns whether the file changed.
pub fn trust(claude_json: &Path, folder: &Path) -> Result<bool> {
    let lock_path = claude_json.with_extension("json.lock");
    if let Some(dir) = lock_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _lock = FileLock::acquire(&lock_path)?;

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
    let count = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = claude_json.with_extension(format!("json.jgt-{}-{count}", std::process::id()));
    let res = (|| -> Result<()> {
        std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
        if let Ok(meta) = std::fs::metadata(claude_json) {
            std::fs::set_permissions(&tmp, meta.permissions())?;
        }
        std::fs::rename(&tmp, claude_json)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res?;
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

/// Repeatedly calls `check` every `interval` until it returns a `Session` or `timeout` expires.
pub async fn poll_coordinator<F, Fut>(
    interval: Duration,
    timeout: Duration,
    mut check: F,
) -> Result<Option<Session>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Option<Session>>>,
{
    let start = tokio::time::Instant::now();
    while start.elapsed() < timeout {
        tokio::time::sleep(interval.min(timeout.saturating_sub(start.elapsed()))).await;
        if let Some(session) = check().await? {
            return Ok(Some(session));
        }
    }
    Ok(None)
}

/// All non-ended Coordinators of `folder`.
pub async fn existing_coordinators(client: &HubClient, folder: &Path) -> Result<Vec<Session>> {
    let all: SessionsResponse = client.get("/v1/sessions?archived=true").await.context("hub unreachable")?;
    let folder_str = folder.to_string_lossy();
    let folder_norm = team::project_folder(&folder_str);
    Ok(all
        .sessions
        .into_iter()
        .filter(|s| {
            s.is_coordinator()
                && s.status != SessionStatus::Ended
                && s.cwd.as_deref().is_some_and(|c| team::project_folder(c) == folder_norm)
        })
        .collect())
}

/// The folder's Coordinator, if one was started before and its context still exists.
async fn existing(client: &HubClient, folder: &Path) -> Result<Option<Session>> {
    Ok(existing_coordinators(client, folder).await?.into_iter().max_by_key(|s| s.last_seen_at))
}

async fn end_coordinator(client: &HubClient, id: &str) -> Result<()> {
    let _: Session = client
        .post(
            &format!("/v1/sessions/{id}/end"),
            &EndRequest { reason: format!("{RETIRED}: duplicate Coordinator ended"), gone: true },
        )
        .await
        .context("hub unreachable")?;
    Ok(())
}

/// Resolves multiple coordinators by keeping the newest-registered (by created_at)
/// and ending any others via `end`. Prints a short note for each ended duplicate.
pub async fn deduplicate_coordinators<FEnd, FutEnd>(
    mut coords: Vec<Session>,
    end: &mut FEnd,
) -> Result<Session>
where
    FEnd: FnMut(&str) -> FutEnd,
    FutEnd: std::future::Future<Output = Result<()>>,
{
    if coords.is_empty() {
        bail!("no Coordinator registered");
    }
    coords.sort_by_key(|s| (s.created_at, s.last_seen_at));
    let newest = coords.pop().unwrap();
    for dup in coords {
        println!("Ended duplicate Coordinator ({}).", dup.id);
        end(&dup.id).await?;
    }
    Ok(newest)
}

/// Polls for a coordinator to register; if not registered within timeout,
/// checks once more immediately before relaunching, then relaunches and polls again.
/// Resolves any duplicate coordinators by keeping only the newest-registered.
pub async fn poll_then_relaunch<FSpawn, FutSpawn, FCheck, FutCheck, FEnd, FutEnd>(
    interval: Duration,
    timeout: Duration,
    mut spawn: FSpawn,
    mut check: FCheck,
    mut end: FEnd,
) -> Result<Session>
where
    FSpawn: FnMut() -> FutSpawn,
    FutSpawn: std::future::Future<Output = Result<()>>,
    FCheck: FnMut() -> FutCheck,
    FutCheck: std::future::Future<Output = Result<Vec<Session>>>,
    FEnd: FnMut(&str) -> FutEnd,
    FutEnd: std::future::Future<Output = Result<()>>,
{
    let first_poll = poll_coordinator(interval, timeout, || {
        let fut = check();
        async move {
            let list = fut.await?;
            Ok(list.into_iter().max_by_key(|s| (s.created_at, s.last_seen_at)))
        }
    })
    .await?;

    if first_poll.is_some() {
        let coords = check().await?;
        return deduplicate_coordinators(coords, &mut end).await;
    }

    // Right before the relaunch, do one last non-waiting check of existing:
    // if a Coordinator appeared in the meantime, skip the relaunch and use it.
    let immediate = check().await?;
    if !immediate.is_empty() {
        return deduplicate_coordinators(immediate, &mut end).await;
    }

    // Relaunch the Coordinator.
    spawn().await?;

    // Second poll.
    let _ = poll_coordinator(interval, timeout, || {
        let fut = check();
        async move {
            let list = fut.await?;
            Ok(list.into_iter().max_by_key(|s| (s.created_at, s.last_seen_at)))
        }
    })
    .await?;

    // After the retry's second poll succeeds (and also right before bailing),
    // list the folder's non-ended Coordinators. If there is more than one,
    // keep the newest-registered one and end the others.
    let coords = check().await?;
    deduplicate_coordinators(coords, &mut end).await
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

    let claude_json = paths::home_dir().join(".claude.json");
    if trust(&claude_json, &folder)? {
        println!("Marked {} as trusted in ~/.claude.json", folder.display());
    }
    let files = prepare_files()?;

    let config = Config::load();
    let agents = TeamAgents::resolve(&config.team, &folder);
    agents.write(&folder).with_context(|| format!("writing .team/agents.json for {}", folder.display()))?;
    println!("{}", agents.describe());

    let claude = launch::find_program("claude", config.commands.claude.as_deref()).map_err(anyhow::Error::msg)?;
    let plan = coordinator_plan(claude.clone(), &folder, &files, &config, None);
    let mut output = tokio::task::spawn_blocking(move || launch::spawn(&plan)).await?.map_err(anyhow::Error::msg)?;

    let claude_for_spawn = claude.clone();
    let folder_for_spawn = folder.clone();
    let files_for_spawn = files.clone();
    let config_for_spawn = config.clone();
    let claude_json_for_spawn = claude_json.clone();
    let relaunch_output = std::sync::Arc::new(std::sync::Mutex::new(None));
    let relaunch_output_for_spawn = relaunch_output.clone();

    let spawn = move || {
        let claude = claude_for_spawn.clone();
        let folder = folder_for_spawn.clone();
        let files = files_for_spawn.clone();
        let config = config_for_spawn.clone();
        let claude_json = claude_json_for_spawn.clone();
        let relaunch_output = relaunch_output_for_spawn.clone();
        async move {
            trust(&claude_json, &folder)?;
            let plan = coordinator_plan(claude, &folder, &files, &config, None);
            let out = tokio::task::spawn_blocking(move || launch::spawn(&plan))
                .await?
                .map_err(anyhow::Error::msg)?;
            *relaunch_output.lock().unwrap() = Some(out);
            Ok(())
        }
    };

    let check = || {
        let client = client.clone();
        let folder = folder.clone();
        async move { existing_coordinators(&client, &folder).await }
    };

    let end = |id: &str| {
        let client = client.clone();
        let id = id.to_string();
        async move { end_coordinator(&client, &id).await }
    };

    let _session = poll_then_relaunch(COORDINATOR_POLL_INTERVAL, COORDINATOR_POLL_TIMEOUT, spawn, check, end)
        .await
        .map_err(|e| {
            if e.to_string().contains("hub unreachable") {
                e
            } else if e.to_string().contains("no Coordinator registered") {
                anyhow::anyhow!(
                    "no Coordinator registered for {}; check ~/.claude.json trust or run `jg-team-dev start` again",
                    folder.display()
                )
            } else {
                e
            }
        })?;

    if let Some(out) = relaunch_output.lock().unwrap().take() {
        output = out;
    }

    println!("Started the Coordinator for {}: {output}", folder.display());
    Ok(())
}

/// `jg-team-dev claim-coordinator`: claims the Coordinator role for this Claude Code session.
pub async fn claim_coordinator() -> Result<()> {
    let client = HubClient::default();
    let pid = procs::harness_pid().context("not started from a Claude Code session")?;
    let s: Session = client
        .get(&format!("/v1/resolve?pid={pid}"))
        .await
        .context("could not resolve Claude Code session for this process")?;
    if s.is_worker() {
        bail!("{}", s.not_coordinator_error());
    }
    let updated: Session = client
        .post(&format!("/v1/sessions/{}/claim-coordinator", s.id), &())
        .await
        .context("failed to claim Coordinator role")?;
    println!("Claimed coordinator role for {}.", updated.display_name());
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
    let cwd = old.cwd.clone().context("the Coordinator has no folder")?;
    let folder = PathBuf::from(team::project_folder(&cwd));
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

    #[test]
    fn an_opencode_worker_gets_its_agent_files_in_our_folder() {
        let files = temp("oc-install");
        prepare_worker_files_in(&files, Harness::Opencode).unwrap();
        for skill in jgt_core::team::WORKER_SKILLS {
            let agent = files.join("opencode/agents").join(format!("jgt-{skill}.md"));
            let text = std::fs::read_to_string(&agent).unwrap_or_else(|e| panic!("{}: {e}", agent.display()));
            assert!(text.starts_with(&format!("---\ndescription: jg-team-dev {skill} worker\n")), "{text}");
        }
        // Workers load them through `OPENCODE_CONFIG_DIR`, our folder; `~/.config/opencode` is untouched.
        assert!(std::fs::read_dir(files.join("opencode")).unwrap().all(|e| e.unwrap().file_name() == "agents"));
        let _ = std::fs::remove_dir_all(&files);
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jgt-team-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn trust_concurrent_calls_preserve_both_folders() {
        let dir = temp("trust-race");
        let file = dir.join(".claude.json");
        std::fs::write(&file, r#"{"projects": {}}"#).unwrap();

        let mut handles = Vec::new();
        for i in 0..20 {
            let file = file.clone();
            let proj = dir.join(format!("proj{i}"));
            handles.push(std::thread::spawn(move || {
                trust(&file, &proj).unwrap()
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let p = v.get("projects").unwrap().as_object().unwrap();
        for i in 0..20 {
            let proj = dir.join(format!("proj{i}"));
            assert!(p.contains_key(&proj.to_string_lossy().into_owned()), "proj{i} missing from ~/.claude.json");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prepare_files_concurrent_calls_produce_valid_files() {
        let dir = temp("prepare-race");
        let mut handles = Vec::new();
        for _ in 0..20 {
            let dir = dir.clone();
            handles.push(std::thread::spawn(move || {
                prepare_files_in(&dir).unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        let settings = std::fs::read_to_string(dir.join("settings.json")).unwrap();
        assert!(serde_json::from_str::<Value>(&settings).is_ok());
        let mcp = std::fs::read_to_string(dir.join("mcp.json")).unwrap();
        assert!(serde_json::from_str::<Value>(&mcp).is_ok());
        let plugin = std::fs::read_to_string(dir.join("plugin/.claude-plugin/plugin.json")).unwrap();
        assert!(serde_json::from_str::<Value>(&plugin).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(args.contains("--settings /data/claude/settings-coordinator.json --mcp-config /data/claude/mcp.json --strict-mcp-config"));
        assert!(args.contains("--setting-sources project,local --plugin-dir /data/claude/plugin"));
        assert!(args.ends_with("-n coordinator · app"));
        assert_eq!(plan.cwd.as_deref(), Some(Path::new("/work/app")));
        assert_eq!(plan.env, vec![("JGT_ROLE".to_string(), "coordinator".to_string())]);
    }

    fn dummy_session(id: &str, created_at: i64) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "harness": "claude-code",
            "external_id": format!("ext-{id}"),
            "status": "idle",
            "listen": false,
            "mirror": false,
            "archived": false,
            "awaiting_reply": false,
            "created_at": created_at,
            "updated_at": created_at,
            "last_seen_at": created_at,
            "role": "coordinator",
            "cwd": "/work/app",
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn poll_coordinator_appears_on_third_poll() {
        let mut count = 0;
        let result = poll_coordinator(
            Duration::from_millis(2),
            Duration::from_millis(100),
            || {
                count += 1;
                let c = count;
                async move {
                    if c >= 3 {
                        Ok(Some(dummy_session("c1", 0)))
                    } else {
                        Ok(None)
                    }
                }
            },
        )
        .await;
        assert!(result.unwrap().is_some());
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn poll_coordinator_never_appears_timeout() {
        let mut count = 0;
        let result = poll_coordinator(
            Duration::from_millis(2),
            Duration::from_millis(10),
            || {
                count += 1;
                async { Ok(None) }
            },
        )
        .await;
        assert!(result.unwrap().is_none());
        assert!(count > 0);
    }

    #[tokio::test]
    async fn poll_coordinator_stops_on_hub_error() {
        let mut count = 0;
        let result = poll_coordinator(
            Duration::from_millis(2),
            Duration::from_millis(100),
            || {
                count += 1;
                async move { Err(anyhow::anyhow!("hub unreachable: connection refused")) }
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(count, 1);
        assert!(result.unwrap_err().to_string().contains("hub unreachable"));
    }

    #[tokio::test]
    async fn late_registration_before_relaunch_no_second_spawn() {
        let mut spawn_count = 0;
        let mut check_count = 0;
        let c1 = dummy_session("c1", 100);

        let result = poll_then_relaunch(
            Duration::from_millis(5),
            Duration::from_millis(10),
            || {
                spawn_count += 1;
                async { Ok(()) }
            },
            || {
                check_count += 1;
                let c = check_count;
                let s = c1.clone();
                async move {
                    // Calls 1 & 2 are during first poll (timeout = 10ms, interval = 5ms).
                    // Call 3 is the non-waiting check right before relaunch.
                    if c >= 3 {
                        Ok(vec![s])
                    } else {
                        Ok(vec![])
                    }
                }
            },
            |_| async { Ok(()) },
        )
        .await;

        let session = result.expect("should find coordinator");
        assert_eq!(session.id, "c1");
        assert_eq!(spawn_count, 0, "relaunch spawn should not have been called");
    }

    #[tokio::test]
    async fn late_registration_after_relaunch_duplicate_ended() {
        let spawn_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawn_called_clone = spawn_called.clone();
        let mut spawn_count = 0;
        let ended = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ended_clone = ended.clone();

        let c1 = dummy_session("c1", 100);
        let c2 = dummy_session("c2", 200);

        let result = poll_then_relaunch(
            Duration::from_millis(2),
            Duration::from_millis(10),
            || {
                spawn_count += 1;
                spawn_called_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                async { Ok(()) }
            },
            || {
                let s1 = c1.clone();
                let s2 = c2.clone();
                let spawned = spawn_called.load(std::sync::atomic::Ordering::SeqCst);
                async move {
                    if spawned {
                        // After relaunch, both coordinators appear!
                        Ok(vec![s1, s2])
                    } else {
                        Ok(vec![])
                    }
                }
            },
            move |id| {
                let ended = ended_clone.clone();
                let id = id.to_string();
                async move {
                    ended.lock().unwrap().push(id);
                    Ok(())
                }
            },
        )
        .await;

        let session = result.expect("should find coordinator");
        assert_eq!(session.id, "c2", "newest coordinator should be kept");
        assert_eq!(spawn_count, 1, "spawn should have been called once on retry");
        assert_eq!(*ended.lock().unwrap(), vec!["c1"], "older duplicate coordinator should be ended");
    }

    #[tokio::test]
    async fn two_coordinators_only_newest_kept() {
        let ended = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let ended_clone = ended.clone();

        let c1 = dummy_session("c1", 100);
        let c2 = dummy_session("c2", 300);

        let result = poll_then_relaunch(
            Duration::from_millis(2),
            Duration::from_millis(10),
            || async { Ok(()) },
            || {
                let s1 = c1.clone();
                let s2 = c2.clone();
                async move { Ok(vec![s1, s2]) }
            },
            move |id| {
                let ended = ended_clone.clone();
                let id = id.to_string();
                async move {
                    ended.lock().unwrap().push(id);
                    Ok(())
                }
            },
        )
        .await;

        let session = result.expect("should return coordinator");
        assert_eq!(session.id, "c2");
        assert_eq!(*ended.lock().unwrap(), vec!["c1"]);
    }

    #[tokio::test]
    async fn hub_error_no_relaunch() {
        let mut spawn_count = 0;

        let result = poll_then_relaunch(
            Duration::from_millis(2),
            Duration::from_millis(10),
            || {
                spawn_count += 1;
                async { Ok(()) }
            },
            || async { Err(anyhow::anyhow!("hub unreachable: connection refused")) },
            |_| async { Ok(()) },
        )
        .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("hub unreachable"));
        assert_eq!(spawn_count, 0, "must never relaunch when hub returns error");
    }

    #[tokio::test]
    async fn deduplicate_three_coordinators_keeps_newest() {
        let mut ended = Vec::new();
        let c1 = dummy_session("c1", 100);
        let c2 = dummy_session("c2", 200);
        let c3 = dummy_session("c3", 300);

        let mut end = |id: &str| {
            ended.push(id.to_string());
            async { Ok(()) }
        };

        let kept = deduplicate_coordinators(vec![c2, c1, c3], &mut end).await.unwrap();
        assert_eq!(kept.id, "c3");
        assert_eq!(ended, vec!["c1", "c2"]);
    }

    #[test]
    fn write_atomic_cleans_up_tmp_file_on_failure() {
        let dir = temp("atomic-cleanup");
        let dest = dir.join("target_dir");
        std::fs::create_dir(&dest).unwrap();
        let res = write_atomic(&dest, "test data");
        assert!(res.is_err());
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("target_dir")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn team_agents_resolve_writes_file_and_locks_settings() {
        let dir = temp("agents-lock");
        let config = Config::default();

        let agents = TeamAgents::resolve(&config.team, &dir);
        agents.write(&dir).unwrap();

        let agents_file = dir.join(".team/agents.json");
        assert!(agents_file.is_file(), "agents.json must be written");

        let desc = agents.describe();
        assert!(desc.starts_with("Agents: coordinator · Claude Code · opus · medium"));
        assert!(desc.contains("coder · Antigravity · gemini-3.8-flash-medium · medium"));

        // Modifying config afterwards does not affect worker settings in this folder:
        let mut modified = config.clone();
        modified.team.skills.insert(
            "coder".into(),
            jgt_core::config::SkillSettings {
                model: Some("sonnet".into()),
                effort: Some("low".into()),
                ..Default::default()
            },
        );

        let (settings, _) = modified.team.for_skill_checked("coder", &dir);
        // Still the saved gemini-3.8-flash-medium / medium on Antigravity:
        assert_eq!(settings.describe("coder"), "coder · Antigravity · gemini-3.8-flash-medium · medium");

        // CLI flags --model and --effort still override (when Antigravity is installed):
        if let Ok((settings, _)) = crate::workers::worker_settings(&modified, "coder", &dir, Some("custom-model"), Some("low"))
            && settings.harness == Harness::Antigravity
        {
            assert_eq!(settings.describe("coder"), "coder · Antigravity · custom-model · low");
        }

        // Starting a new team (rewriting the file with modified config) updates it:
        let new_agents = TeamAgents::resolve(&modified.team, &dir);
        new_agents.write(&dir).unwrap();
        let (settings, _) = modified.team.for_skill_checked("coder", &dir);
        assert_eq!(settings.describe("coder"), "coder · Claude Code · sonnet · low");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
