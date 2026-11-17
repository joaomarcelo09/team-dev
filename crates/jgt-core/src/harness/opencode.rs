//! OpenCode workers (PRD H5).
//!
//! Checked on this machine with `opencode run --help` (v2.3.0) and smoke runs:
//! `opencode run [message]` runs headless; `--format json` prints one JSON event per line:
//! `{"type": "text", "sessionID", "part": {"messageID", "text"}}` for the reply and
//! `{"type": "step_finish", "part": {"tokens": {"input", "output", …}, "cost"}}` per step.
//! `--standalone` (avoids shared background service that ignores config env vars),
//! `--model provider/model#variant`, `--agent`, `--title`, `--auto` (approve what is not
//! denied), and `--session` exist. `OPENCODE_CONFIG_DIR` and `OPENCODE_CONFIG_CONTENT` are
//! read from the environment. OpenCode v2.3 takes its working directory from the `PWD`
//! environment variable, not from the process cwd (a stale inherited `PWD` makes files land in
//! the wrong folder), so the worker env sets `PWD` to the worker cwd.
//!
//! The reasoning variant is part of the model spec (`--model provider/model#variant`); v2.3 has
//! no `--variant` flag and no `models --verbose` any more. The model registry it serves at
//! `opencode api --standalone GET /api/model` lists each model's `variants`, which is what that
//! spec validates against: an unsupported variant fails with `provider.no-route`.
//!
//! Our files live under jg-team-dev's own data folder: the user's `~/.config/opencode` (and its
//! plugins) is never touched; worker cwd is set on the spawned process.

use super::{Caps, HarnessAdapter, Outcome, WorkerLaunch, brief_prompt, effort_levels, skill_text};
use crate::{
    config::Config,
    launch::{LaunchPlan, find_program},
    model::{Harness, Session},
    team::{SESSION_ENV, WORKER_SKILLS},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

pub struct Opencode;

/// Extracts model identifiers from `opencode models` output.
///
/// Keeps lines that contain `/` (`provider/model`), ignoring header or status lines.
pub fn parse_models(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.contains('/') {
                return None;
            }
            let token = line.split_whitespace().find(|w| w.contains('/'))?;
            Some(token.to_string())
        })
        .collect()
}

/// Extracts every model's reasoning variants from `opencode api --standalone GET /api/model`.
///
/// Keys are `provider/model`, values the variant ids, lowest first. Entries without a provider,
/// a model or variants are skipped; a model that reports no variants maps to an empty list.
pub fn parse_variants(output: &str) -> BTreeMap<String, Vec<String>> {
    let Ok(value) = serde_json::from_str::<Value>(output) else {
        return BTreeMap::new();
    };
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .or_else(|| value.as_array());
    let Some(entries) = entries else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for entry in entries {
        let provider = entry.get("providerID").and_then(Value::as_str);
        let model = entry
            .get("modelID")
            .or_else(|| entry.get("id"))
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty());
        let (Some(provider), Some(model)) = (provider, model) else {
            continue;
        };
        let mut levels: Vec<String> = entry
            .get("variants")
            .and_then(Value::as_array)
            .map(|variants| {
                variants
                    .iter()
                    .filter_map(|v| {
                        v.get("id")
                            .and_then(Value::as_str)
                            .or_else(|| v.as_str())
                            .map(str::to_owned)
                    })
                    .collect()
            })
            .unwrap_or_default();
        super::sort_efforts(&mut levels);
        levels.dedup();
        out.insert(format!("{provider}/{model}"), levels);
    }
    out
}

/// The model registry v2.3 serves at `/api/model`, parsed into each model's variants.
fn run_variants(program: &Path) -> Result<BTreeMap<String, Vec<String>>, String> {
    let stdout = super::run_with_timeout(
        program,
        &["api", "--standalone", "GET", "/api/model"],
        super::MODELS_TIMEOUT,
    )?;
    Ok(parse_variants(&stdout))
}

/// The model registry, or an empty map when OpenCode cannot answer: then no variant is passed.
fn variants(program: &Path) -> BTreeMap<String, Vec<String>> {
    run_variants(program).unwrap_or_default()
}

/// `<files>/opencode`: our OpenCode config folder (`OPENCODE_CONFIG_DIR`), with one agent per skill.
fn config_dir(files: &Path) -> PathBuf {
    files.join("opencode")
}

/// The agent a worker with `skill` runs as.
fn agent(skill: &str) -> String {
    format!("jgt-{skill}")
}

/// What `OPENCODE_CONFIG_CONTENT` adds for a worker: our MCP server, told the worker's id, and
/// deny rules for the git commands only the git-coder may run (PRD Q8: no routing, `--auto`).
fn inline_config(exe: &Path, session_id: &str, skill: &str) -> Value {
    let mut bash = serde_json::Map::new();
    if !crate::team::may_run_git(skill) {
        // Checked with `opencode debug agent`: each becomes a `bash` rule with this glob pattern,
        // after the `*` allow, so it wins.
        for cmd in crate::team::GIT_WRITE_GLOBS {
            bash.insert((*cmd).into(), json!("deny"));
        }
    }
    // Everything else is approved by `--auto`, so no catch-all rule (whose place in the
    // matching order is unverified).
    json!({
        "mcp": {
            crate::team::PLUGIN_NAME: {
                "type": "local",
                "command": [exe.to_string_lossy(), "mcp", "--harness", "opencode"],
                "environment": { SESSION_ENV: session_id },
                "enabled": true,
            }
        },
        // Workers read this repository locally (`git show`), never over the web.
        "permission": { "bash": bash, "webfetch": "deny" },
    })
}

impl HarnessAdapter for Opencode {
    fn harness(&self) -> Harness {
        Harness::Opencode
    }

    fn caps(&self) -> Caps {
        Caps {
            coordinator: false,
            registers_itself: false,
            permission_routing: false,
            mcp: true,
            effort_levels: effort_levels(Harness::Opencode),
            reports_cost: true,
            run_timeout: Some(Duration::from_secs(2 * 3600)),
        }
    }

    fn find(&self, config: &Config) -> Result<PathBuf, String> {
        find_program("opencode", config.commands.opencode.as_deref())
    }

    /// One agent file per worker skill (PRD Q9), from the skills Claude Code's install wrote.
    fn install(&self, files: &Path) -> std::io::Result<()> {
        let agents = config_dir(files).join("agents");
        std::fs::create_dir_all(&agents)?;
        for skill in WORKER_SKILLS {
            // The CLI that writes the plugin may not bundle every worker skill in this version
            // (the git-coder skill comes in its own piece); install the ones it wrote.
            let body = match skill_text(files, skill) {
                Ok(body) => body,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            // `opencode debug agent` reads these as name, description, mode and the body as the prompt.
            let text = format!("---\ndescription: jg-team-dev {skill} worker\nmode: primary\n---\n\n{body}\n");
            std::fs::write(agents.join(format!("{}.md", agent(skill))), text)?;
        }
        Ok(())
    }

    fn worker(&self, w: &WorkerLaunch) -> Result<LaunchPlan, String> {
        let mut args: Vec<String> = vec!["run".into(), "--standalone".into(), "--format".into(), "json".into()];
        args.extend(["--agent".into(), agent(w.skill)]);
        // OpenCode names models `provider/model`; anything else (a Claude alias) is left to its default.
        // The effort (reasoning variant) is appended to that spec as `#variant`, but only when the
        // model reports it: v2.3 fails a worker with `provider.no-route` on an unknown variant, and
        // an old saved or default effort must not do that.
        if w.settings.model.contains('/') {
            let level = w.settings.effort.trim();
            let supported = !level.is_empty()
                && variants(&w.program)
                    .get(&w.settings.model)
                    .is_some_and(|levels| levels.iter().any(|l| l == level));
            let model_args = if supported { self.effort_args(level, &w.settings.model) } else { None };
            match model_args {
                Some(model_args) => args.extend(model_args),
                None => args.extend(["--model".into(), w.settings.model.clone()]),
            }
        }
        args.extend(["--title".into(), w.title.to_string(), "--auto".into()]);
        args.push(brief_prompt(w.brief, w.brief_file));
        let mut env = w.env.clone();
        env.push(("OPENCODE_CONFIG_DIR".into(), config_dir(w.files).to_string_lossy().into_owned()));
        // OpenCode v2.3 reads its working directory from `PWD`, not the process cwd.
        env.push(("PWD".into(), w.cwd.to_string_lossy().into_owned()));
        env.push(("OPENCODE_CONFIG_CONTENT".into(), inline_config(&w.exe, w.session_id, w.skill).to_string()));
        env.push((SESSION_ENV.into(), w.session_id.to_string()));
        Ok(LaunchPlan {
            program: w.program.clone(),
            args,
            cwd: Some(w.cwd.to_path_buf()),
            // `opencode` may be a `#!/usr/bin/env node` script next to its `node` (nvm installs).
            path_prepend: w.program.parent().map(Path::to_path_buf),
            wait: true,
            log: None,
            env,
            env_remove: vec![],
        })
    }

    /// Our id is not OpenCode's session id (`ses_…`), so a finished worker is not resumed; the
    /// Coordinator starts a fresh one.
    fn resume(&self, _s: &Session, _prompt: &str, _config: &Config) -> Result<LaunchPlan, String> {
        Err("OpenCode workers are not resumed; start a fresh worker".into())
    }

    /// The text of the last message that has text, and the tokens and cost of every step.
    fn parse_result(&self, stdout: &str, stderr: &str, ok: bool) -> Outcome {
        let events: Vec<Value> = stdout.lines().filter_map(|l| serde_json::from_str(l.trim()).ok()).collect();
        if events.is_empty() {
            let text = if stdout.trim().is_empty() { stderr.trim() } else { stdout.trim() };
            return Outcome { ok: false, text: text.to_string(), ..Outcome::default() };
        }
        let kind = |e: &Value| e.get("type").and_then(Value::as_str).unwrap_or("").to_string();
        let part = |e: &Value, k: &str| e.get("part").and_then(|p| p.get(k)).cloned();
        let last_message = events
            .iter()
            .filter(|e| kind(e) == "text")
            .filter_map(|e| part(e, "messageID").and_then(|m| m.as_str().map(str::to_owned)))
            .next_back();
        let text: Vec<String> = events
            .iter()
            .filter(|e| kind(e) == "text")
            .filter(|e| part(e, "messageID").and_then(|m| m.as_str().map(str::to_owned)) == last_message)
            .filter_map(|e| part(e, "text").and_then(|t| t.as_str().map(str::to_owned)))
            .collect();
        let steps: Vec<&Value> = events.iter().filter(|e| kind(e) == "step_finish").collect();
        let sum_tokens = |k: &str| {
            let values: Vec<i64> =
                steps.iter().filter_map(|e| part(e, "tokens").and_then(|t| t.get(k).and_then(Value::as_i64))).collect();
            (!values.is_empty()).then(|| values.iter().sum())
        };
        let costs: Vec<f64> = steps.iter().filter_map(|e| part(e, "cost").and_then(|c| c.as_f64())).collect();
        // An error event (seen with an unknown model, exit 1):
        // `{"type": "error", "error": {"name": "UnknownError", "data": {"message": "…"}}}`
        // or v2.3 `{"type": "error", "error": {"type": "provider.no-route", "message": "…"}}`.
        let error = events.iter().find(|e| kind(e) == "error").map(|e| {
            let err = e.get("error");
            let field = |k: &str| err.and_then(|v| v.get(k)).and_then(Value::as_str);
            let message = err
                .and_then(|v| v.pointer("/data/message").or_else(|| v.get("message")))
                .and_then(Value::as_str);
            let name = field("name").or_else(|| field("type"));
            match (name, message) {
                (Some(name), Some(m)) => format!("{name}: {m}"),
                (None, Some(m)) => m.to_string(),
                _ => err.or_else(|| e.get("part")).map_or_else(|| e.to_string(), Value::to_string),
            }
        });
        Outcome {
            ok: ok && error.is_none() && !text.is_empty(),
            text: match (&error, text.is_empty()) {
                (Some(err), _) => err.clone(),
                (None, true) => "(no reply)".into(),
                (None, false) => text.join(""),
            },
            cost_usd: (!costs.is_empty()).then(|| costs.iter().sum()),
            tokens_in: sum_tokens("input"),
            tokens_out: sum_tokens("output"),
            conversation: events.iter().find_map(|e| e.get("sessionID").and_then(Value::as_str).map(str::to_owned)),
        }
    }

    /// v2.3 takes the reasoning variant in the model spec, not in a flag: `--model
    /// provider/model#variant`. An empty effort (or a non-`provider/model` alias) leaves the
    /// model untouched and lets OpenCode use its default.
    fn effort_args(&self, level: &str, model: &str) -> Option<Vec<String>> {
        let level = level.trim();
        (model.contains('/') && !level.is_empty())
            .then(|| vec!["--model".into(), format!("{model}#{level}")])
    }

    fn models(&self, config: &Config) -> Result<Vec<String>, String> {
        let program = self.find(config)?;
        let stdout = super::run_with_timeout(&program, &["models"], super::MODELS_TIMEOUT)?;
        Ok(parse_models(&stdout))
    }

    /// The reasoning variants each model reports, by `provider/model` (v2.3's model registry).
    fn model_efforts(&self, config: &Config) -> Result<BTreeMap<String, Vec<String>>, String> {
        let program = self.find(config)?;
        run_variants(&program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_models_extracts_slash_containing_lines_and_skips_headers() {
        let sample = "\
OpenCode 1.18.35
Available models:
anthropic/claude-3-5-sonnet
openai/gpt-4o
google/gemini-1.5-pro
deepseek/deepseek-chat
";
        let models = parse_models(sample);
        assert_eq!(
            models,
            [
                "anthropic/claude-3-5-sonnet",
                "openai/gpt-4o",
                "google/gemini-1.5-pro",
                "deepseek/deepseek-chat",
            ]
        );
    }

    #[test]
    fn parse_models_handles_descriptions_and_whitespace() {
        let sample = "  anthropic/claude-3-5-sonnet (recommended)  \nopenai/gpt-4o\tDefault\n";
        assert_eq!(parse_models(sample), ["anthropic/claude-3-5-sonnet", "openai/gpt-4o"]);
    }

    #[test]
    fn parse_variants_reads_each_models_effort_levels() {
        // `opencode api --standalone GET /api/model` in v2.3 (trimmed to the fields we read).
        let sample = r#"{"location":{"directory":"/tmp"},"data":[
{"id":"deepseek-v4.1-flash","modelID":"deepseek-v4.1-flash","providerID":"opencode-go",
 "variants":[{"id":"max","settings":{}},{"id":"low","settings":{}},{"id":"high","settings":{}}]},
{"id":"glm-5.2","modelID":"glm-5.2","providerID":"opencode-go",
 "variants":[{"id":"high"},{"id":"max"}]},
{"id":"kimi-k2.7-code","modelID":"kimi-k2.7-code","providerID":"opencode-go"},
{"id":"legacy","providerID":"opencode-go","variants":["high","low"]},
{"modelID":"orphan","variants":[{"id":"low"}]}
]}"#;
        let variants = parse_variants(sample);
        let levels = |key: &str| variants.get(key).map(|v| v.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(levels("opencode-go/deepseek-v4.1-flash"), Some(vec!["low", "high", "max"]));
        assert_eq!(levels("opencode-go/glm-5.2"), Some(vec!["high", "max"]));
        assert_eq!(levels("opencode-go/kimi-k2.7-code"), Some(vec![]));
        assert_eq!(levels("opencode-go/legacy"), Some(vec!["low", "high"]));
        assert!(!variants.contains_key("opencode-go/orphan"));
        assert!(parse_variants("not json").is_empty());
    }

    #[test]
    fn the_effort_is_a_variant_of_the_model_spec() {
        let args = |level: &str, model: &str| Opencode.effort_args(level, model);
        assert_eq!(
            args("max", "opencode-go/deepseek-v4.1-flash"),
            Some(vec!["--model".into(), "opencode-go/deepseek-v4.1-flash#max".into()])
        );
        assert_eq!(
            args("high", "opencode-go/deepseek-v4.1-flash"),
            Some(vec!["--model".into(), "opencode-go/deepseek-v4.1-flash#high".into()])
        );
        assert_eq!(args("", "opencode-go/deepseek-v4.1-flash"), None);
        // A Claude alias is left to OpenCode's default.
        assert_eq!(args("high", "sonnet"), None);
    }

    #[cfg(unix)]
    #[test]
    fn workers_pass_a_variant_the_model_reports_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("jgt-oc-variants-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("opencode");
        std::fs::write(
            &program,
            "#!/bin/sh\ncat <<'JSON'\n{\"data\":[{\"id\":\"deepseek-v4.1-flash\",\"providerID\":\"opencode-go\",\"variants\":[{\"id\":\"low\"},{\"id\":\"high\"},{\"id\":\"max\"}]}]}\nJSON\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut settings = Config::default().team.for_skill("reviewer", Path::new("/nonexistent"));
        settings.harness = Harness::Opencode;
        settings.model = "opencode-go/deepseek-v4.1-flash".into();
        let mut launch = |effort: &str| {
            settings.effort = effort.into();
            Opencode
                .worker(&WorkerLaunch {
                    program: program.clone(),
                    exe: "/bin/jg-team-dev".into(),
                    files: &dir,
                    skill: "reviewer",
                    title: "reviewer · x",
                    brief: "Review it.",
                    brief_file: Path::new("/b.md"),
                    settings: &settings,
                    cwd: Path::new("/w"),
                    session_id: "uuid",
                    env: vec![],
                })
                .unwrap()
        };
        let model_arg = |args: &[String]| {
            args.iter().position(|a| a == "--model").map(|i| args[i + 1].clone())
        };

        assert_eq!(model_arg(&launch("max").args).as_deref(), Some("opencode-go/deepseek-v4.1-flash#max"));
        // `medium` is our default effort, but this model does not report it: no variant.
        assert_eq!(model_arg(&launch("medium").args).as_deref(), Some("opencode-go/deepseek-v4.1-flash"));
        assert_eq!(model_arg(&launch("turbo").args).as_deref(), Some("opencode-go/deepseek-v4.1-flash"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_smoke_run_output_parses() {
        let out = r#"{"type":"step_start","timestamp":1,"sessionID":"ses_e81c","part":{"messageID":"msg_1","type":"step-start"}}
{"type":"text","timestamp":1,"sessionID":"ses_e81c","part":{"messageID":"msg_1","type":"text","text":"ok"}}
{"type":"step_finish","timestamp":1,"sessionID":"ses_e81c","part":{"reason":"stop","messageID":"msg_1","type":"step-finish","tokens":{"total":20168,"input":18374,"output":2,"reasoning":0,"cache":{"write":0,"read":1792}},"cost":0}}"#;
        let o = Opencode.parse_result(out, "", true);
        assert_eq!((o.ok, o.text.as_str(), o.cost_usd, o.tokens_in, o.tokens_out), (true, "ok", Some(0.0), Some(18374), Some(2)));
        assert_eq!(o.conversation.as_deref(), Some("ses_e81c"));
        // Several messages: the reply is the last one's text.
        let two = r#"{"type":"text","part":{"messageID":"m1","text":"thinking about it"}}
{"type":"text","part":{"messageID":"m2","text":"done "}}
{"type":"text","part":{"messageID":"m2","text":"```json\n{}\n```"}}"#;
        assert_eq!(Opencode.parse_result(two, "", true).text, "done ```json\n{}\n```");
        assert!(!Opencode.parse_result(r#"{"type":"error","error":{"message":"bad model"}}"#, "", true).ok);
        let err = r#"{"type":"error","timestamp":1,"sessionID":"ses_e815","error":{"name":"UnknownError","data":{"message":"Unexpected server error. Check server logs for details.","ref":"err_c7fd"}}}"#;
        let o = Opencode.parse_result(err, "", false);
        assert_eq!((o.ok, o.text.as_str()), (false, "UnknownError: Unexpected server error. Check server logs for details."));
        let none = Opencode.parse_result("", "Error: provider not configured", false);
        assert_eq!((none.ok, none.text.as_str()), (false, "Error: provider not configured"));

        // OpenCode v2.3 real run output
        let v23 = "{\"type\":\"step_start\",\"timestamp\":1794861181996,\"sessionID\":\"ses_e19efd5b2ffefFFAG0umlkLWp0\",\"part\":{\"id\":\"prt_1\",\"sessionID\":\"ses_e19efd5b2ffefFFAG0umlkLWp0\",\"messageID\":\"msg_1\",\"type\":\"step-start\"}}\n\
{\"type\":\"text\",\"timestamp\":1794861182101,\"sessionID\":\"ses_e19efd5b2ffefFFAG0umlkLWp0\",\"part\":{\"id\":\"prt_2\",\"sessionID\":\"ses_e19efd5b2ffefFFAG0umlkLWp0\",\"messageID\":\"msg_1\",\"type\":\"text\",\"text\":\"combined_ok\"}}\n\
{\"type\":\"step_finish\",\"timestamp\":1794861182108,\"sessionID\":\"ses_e19efd5b2ffefFFAG0umlkLWp0\",\"part\":{\"id\":\"prt_3\",\"sessionID\":\"ses_e19efd5b2ffefFFAG0umlkLWp0\",\"messageID\":\"msg_1\",\"type\":\"step-finish\",\"reason\":\"stop\",\"cost\":0,\"tokens\":{\"input\":3346,\"output\":3,\"reasoning\":27,\"cache\":{\"read\":3904,\"write\":0}}}}";
        let o23 = Opencode.parse_result(v23, "", true);
        assert_eq!((o23.ok, o23.text.as_str(), o23.cost_usd, o23.tokens_in, o23.tokens_out), (true, "combined_ok", Some(0.0), Some(3346), Some(3)));
        assert_eq!(o23.conversation.as_deref(), Some("ses_e19efd5b2ffefFFAG0umlkLWp0"));

        let err_v23 = r#"{"type":"error","timestamp":1794861204926,"sessionID":"ses_e19ef7a5affejSzt6NdXD4wZro","error":{"type":"provider.no-route","message":"Model unavailable: invalid/nonexistent"}}"#;
        let o_err23 = Opencode.parse_result(err_v23, "", false);
        assert_eq!((o_err23.ok, o_err23.text.as_str()), (false, "provider.no-route: Model unavailable: invalid/nonexistent"));
    }

    #[test]
    fn install_skips_skills_the_cli_has_not_written_yet() {
        let files = std::env::temp_dir().join(format!("jgt-oc-skip-{}", std::process::id()));
        let coder = files.join("plugin/skills/coder");
        std::fs::create_dir_all(&coder).unwrap();
        std::fs::write(coder.join("SKILL.md"), "---\nname: coder\n---\n\n# coder\n").unwrap();
        Opencode.install(&files).unwrap();
        assert!(files.join("opencode/agents/jgt-coder.md").is_file());
        assert!(!files.join("opencode/agents/jgt-git-coder.md").exists());
        let _ = std::fs::remove_dir_all(&files);
    }

    #[test]
    fn only_the_git_coder_may_run_git_writes_and_gh() {
        let inline = |skill: &str| {
            let mut settings = Config::default().team.for_skill(skill, Path::new("/nonexistent"));
            settings.harness = Harness::Opencode;
            let w = WorkerLaunch {
                program: "/bin/opencode".into(),
                exe: "/bin/jg-team-dev".into(),
                files: Path::new("/f"),
                skill,
                title: "t",
                brief: "b",
                brief_file: Path::new("/b.md"),
                settings: &settings,
                cwd: Path::new("/w"),
                session_id: "uuid",
                env: vec![],
            };
            let plan = Opencode.worker(&w).unwrap();
            let text = plan.env.iter().find(|(k, _)| k == "OPENCODE_CONFIG_CONTENT").unwrap().1.clone();
            serde_json::from_str::<Value>(&text).unwrap()
        };

        // Every other worker is denied the git write steps and gh.
        for skill in ["coder", "reviewer"] {
            let config = inline(skill);
            for glob in crate::team::GIT_WRITE_GLOBS {
                assert_eq!(config["permission"]["bash"][*glob], "deny", "{skill}: {glob}");
            }
            // The documented worktree and option forms are covered.
            assert_eq!(config["permission"]["bash"]["git -C * commit*"], "deny", "{skill}");
            assert_eq!(config["permission"]["bash"]["git -c * push*"], "deny", "{skill}");
            assert_eq!(config["permission"]["bash"]["git --no-pager merge*"], "deny", "{skill}");
        }
        // The git-coder has no deny rule; `--auto` lets it run them.
        let git_coder = inline("git-coder");
        assert_eq!(git_coder["permission"]["bash"], json!({}));
    }

    #[test]
    fn workers_use_our_config_dir_and_mcp_server() {
        let files = std::env::temp_dir().join(format!("jgt-oc-{}", std::process::id()));
        for skill in WORKER_SKILLS {
            let dir = files.join("plugin/skills").join(skill);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), format!("---\nname: {skill}\n---\n\n# {skill}\n")).unwrap();
        }
        Opencode.install(&files).unwrap();
        let agent = std::fs::read_to_string(files.join("opencode/agents/jgt-coder.md")).unwrap();
        assert!(agent.starts_with("---\ndescription: jg-team-dev coder worker\nmode: primary\n---\n\n# coder"), "{agent}");

        let mut settings = Config::default().team.for_skill("reviewer", Path::new("/nonexistent"));
        settings.harness = Harness::Opencode;
        settings.model = "deepseek/deepseek-chat".into();
        let w = WorkerLaunch {
            program: "/nvm/bin/opencode".into(),
            exe: "/bin/jg-team-dev".into(),
            files: &files,
            skill: "reviewer",
            title: "reviewer · x",
            brief: "Review it.",
            brief_file: Path::new("/b.md"),
            settings: &settings,
            cwd: Path::new("/w"),
            session_id: "uuid",
            env: vec![],
        };
        let plan = Opencode.worker(&w).unwrap();
        assert_eq!(
            plan.args,
            [
                "run", "--standalone", "--format", "json", "--agent", "jgt-reviewer", "--model",
                "deepseek/deepseek-chat", "--title", "reviewer · x", "--auto", "Review it."
            ]
        );
        let env = |k: &str| plan.env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).unwrap();
        assert_eq!(env("OPENCODE_CONFIG_DIR"), files.join("opencode").to_string_lossy());
        assert_eq!(env("PWD"), "/w");
        assert_eq!(env(SESSION_ENV), "uuid");
        let inline: Value = serde_json::from_str(&env("OPENCODE_CONFIG_CONTENT")).unwrap();
        assert_eq!(inline["mcp"]["jg-team-dev"]["command"], json!(["/bin/jg-team-dev", "mcp", "--harness", "opencode"]));
        assert_eq!(inline["mcp"]["jg-team-dev"]["environment"][SESSION_ENV], "uuid");
        assert_eq!(inline["permission"]["bash"]["git push*"], "deny");
        assert_eq!(inline["permission"]["webfetch"], "deny");
        assert_eq!(plan.path_prepend.as_deref(), Some(Path::new("/nvm/bin")));
        let _ = std::fs::remove_dir_all(&files);
    }
}
