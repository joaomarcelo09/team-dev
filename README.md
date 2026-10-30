# jg-team-dev

A desktop chat with a **Coordinator**: a Claude Code session that listens to you and runs a team
of worker sessions (planner, coder, reviewer, tester, git-coder). Each worker starts with a
clean context and one skill, reports progress in its own chat box, and exits when its task is done.

```
you ⇄ jg-team-dev app ⇄ jg-team-dev-hub (SQLite, unix socket) ⇄ MCP + hooks ⇄ Coordinator ─ claude -p ─▶ workers
```

Product requirements: [PRD — jg-team-dev](https://claude.ai/code/artifact/06b0ae58-d1b1-414b-9d9e-57b3258b0483).

The messaging layer (hub, MCP server, hooks, iced app) is ported from
[Liaison](../chat-ai); the roles come from the [Adaptive AI Team Harness](../team-dev). This
repository depends on neither, and runs side by side with Liaison: its own socket, database,
config, and its hooks are passed to sessions with `--settings` instead of being installed into
`~/.claude/settings.json`.

## Status

- **Phase 0** (workspace): Liaison ported and renamed; its own socket, database and config.
- **Phase 1** (Coordinator chat): `jg-team-dev start [folder]` (or **New team** in the app) marks
  the folder trusted and starts a background Coordinator with jg-team-dev's hooks, MCP server
  and skills. It listens for 50 minutes (`[timeouts] listen_secs`), then its process stops; your
  next message wakes it with its context intact.

- **Phase 2** (workers): the Coordinator runs `jg-team-dev worker <skill> --task … --brief-file …`
  (planner, coder, reviewer, tester, git-coder). It returns at once; a detached process runs
  `claude -p "/jg-team-dev:<skill> <brief>"` and relays the result JSON to the Coordinator
  through the hub, waking it if needed. Workers get their own chats under their Coordinator;
  messaging a finished worker resumes it for one reply, and the Coordinator gets a summary.
  `jg-team-dev workers` lists them (the Coordinator keeps at most 3 running).

- **Phase 3** (git and memory): coders work in `.team/worktrees/<name>` on branch `jgt/<name>`
  (`jg-team-dev worker coder --worktree <name>`; reviewers and testers can point at the same
  worktree). In a git repository a coder without a worktree is refused unless the Coordinator
  passes `--main-worktree` after warning you. `.team/` is kept out of `git status` through the
  repository's local exclude file. The git-coder merges into the main worktree only after a
  review, a test and your OK, then removes the worktree. On a topic change the Coordinator asks,
  writes `.team/tasks/task-<date>-<slug>.md`, and runs `jg-team-dev reset --summary <file>`: a
  fresh Coordinator takes over the folder and the old chat is archived, read-only.

- **Phase 4** (polish): each request with workers is a *run*. `jg-team-dev run start` writes
  `.team/runs/<run-id>/request.md`; workers append to `events.jsonl` and save their result in
  `worker-results/`; `jg-team-dev run finish` writes `final-report.md` and archives the run's
  worker chats. `jg-team-dev run list` shows a project's runs. A Coordinator's chat has a **Team**
  panel: each worker's state, time and cost; click one to open its chat. Model, effort,
  permission mode and tools can be set per skill and per project (below).

- **Kanban (K-1, board data)**: each workspace (a Coordinator's folder) has a Board of tasks in
  the hub database, `JGT-1`, `JGT-2`, … per workspace (keys are never reused), in seven columns:
  Backlog, On Queue, Working, Testing, Review, Orchestrator review, Done. Every move, edit, result
  and comment goes to the task's activity log with who made it. The hub enforces the gates:
  workers only read the Board; agents never edit the title, description or priority; the
  Coordinator creates tasks in Backlog and queues only subtasks of a task you queued; at most
  `wip_limit` tasks are in progress. The flow is coder → verifier → one fix round → verifier →
  Orchestrator review. A task whose first verification is clean (green test, approved review)
  goes Review → Done. Otherwise it gets one automatic fix round (back to Working) and a second
  verification, then waits in **Orchestrator review**, where only you decide: done, one more fix
  (one more coder and verifier, then back to Orchestrator review), take over, or stop. Findings
  that are not critical or high go to a follow-up task in Backlog and never start a fix round. You
  may move any task anywhere; a move that skips a gate is logged as "moved by you". Reopening a
  Done task resets its rounds. `jg-team-dev task` (`list`, `show`, `create`, `edit`, `details`, `move`,
  `result`, `decide`, `comment`, `delete`, `log`) drives it from a terminal or a session.

- **Kanban (K-2, Board view)**: a Coordinator's chat header has two tabs, **Chat** (the default)
  and **Board**. The Board shows the workspace's seven columns with their counts, scrolling
  sideways in a narrow window; Done shows the 20 newest tasks, with "Show all". "+" in Backlog
  and On Queue adds a task (a title is enough). A card shows its key, title, priority, the worker
  on it (`↳ verifier`), the verification (`round 1/2`) and *Waiting for you* or *Waiting: usage
  limit*; move it with ← → or
  "Move to…". Click a card for its panel: edit the title, description and priority; read the
  Coordinator's Details, the test and review results (critical findings first) and the activity
  log; open the run folder or a worker's chat; decide a task in Orchestrator review (done, one
  more fix, take over, stop); delete it. Changes from the team or a terminal show up live.

- **Kanban (K-3, team on the board)**: moving a task to On Queue is the request. The hub queues
  `[jg-team-dev] Task JGT-n "…" is on the queue` for the folder's current Coordinator, and the
  app or the CLI that moved it wakes a sleeping Coordinator (`team::wake`, shared with worker
  results; the hub never starts agents). With no Coordinator running, the task's log says so and
  the app offers "Start team". The Coordinator works the task with the MCP tools `task_list`,
  `task_get`, `task_update`, `task_move`, `task_create`, `task_decide` and `task_log`, following
  the "Board" section of its skill: it moves the card before announcing each step and keeps the
  one-fix-round rule. Workers only read (`task_get`). `jg-team-dev run start --task-id JGT-n` names the
  run after the key and links it to the task; `jg-team-dev worker … --task-id JGT-n` works in
  `.team/worktrees/jgt-n` (branch `jgt/jgt-n`) in the task's run, and the card shows the worker
  on it. The verifier and the reviewer tag each finding by severity, with a category (security,
  secret, performance, patterns, docs, behavior); only critical and high findings start a fix round.

- **Kanban (K-4, polish)**: drag a card to another column or position (Esc cancels; ← →
  and "Move to…" stay). Comment on a task from its panel or `jg-team-dev task comment`: the
  comment reaches the Coordinator like a chat message and wakes it. A task reaching Done, or
  getting *Waiting for you*, posts a notification in the Coordinator's chat, which also shows on
  the desktop. Done tasks are archived (kept, hidden; `task list --all` shows them) 7 days after
  they got there; moving one brings it back. Export the Board as Markdown with the Board's
  Export button or `jg-team-dev task export [--out FILE] [--all]` (default `docs/board.md`).

- **Team efficiency**: after a coder the Coordinator starts one **verifier**, the default gate:
  build, tests, the acceptance checks and a short review for critical and high problems, in one
  pass. A tester and a reviewer run only when you ask for them (or the task needs it). Fix rounds
  check only the new diff: `jg-team-dev snapshot --task JGT-n` saves what a verification checked
  as a git tree under `refs/jgt/<key>/checked-<round>` (no commit, the branch and index stay
  untouched), and `jg-team-dev snapshot --task JGT-n --diff` prints the worktree's diff since
  then. A finished worker's result reaches the Coordinator as a short digest (at most 10 lines:
  status, finding counts, notes) with the path of the full result in the run, which also shows
  in the worker's chat. A worker stopped by the usage limit sets *Waiting: usage limit* on its
  task with the reset time; the Coordinator starts no worker meanwhile, and shortly after the
  reset the hub clears it and wakes the Coordinator, which resumes the task with a fresh worker
  from the saved resume brief. `jg-team-dev worker … --model <m> --effort <e>` raises one worker
  for a hard task without changing the config.

Team sessions use `[team]` in `config.toml`. Each role runs on its own harness, model and
effort: its built-in default, shown below as the config that spells it out. A
`[team.skills.<role>]` table you write replaces the role's harness, model and effort as a whole:

```toml
[team]
# harness = "claude-code"  # claude-code, opencode or antigravity (workers only)
# model = "opus"           # below the role defaults: set a role's table to change that role
# effort = "medium"        # low, medium, high, xhigh, max (checked against the harness)
permission_mode = "auto"   # only prompts auto mode can't settle reach the app
wip_limit = 1              # Board tasks in progress at once (1..=3)

[team.skills.coordinator]  # Claude Code only
model = "opus"
effort = "medium"

[team.skills.planner]
model = "opus"
effort = "medium"

[team.skills.coder]
harness = "antigravity"           # Google Antigravity (`agy`)
model = "gemini-3.8-flash-high"   # Gemini 3.8 Flash, as `agy models` names it
effort = "high"
# permission_mode = "bypassPermissions"  # agy --dangerously-skip-permissions: see below
allowed_tools = ["Bash(cargo test:*)"]   # claude --allowedTools (Claude Code only)
disallowed_tools = ["WebFetch"]          # claude --disallowedTools (Claude Code only)

[team.skills.coder.fallback]      # used where `agy` is not installed
harness = "claude-code"
model = "sonnet"
effort = "medium"

[team.skills.verifier]     # the default gate; also [team.skills.reviewer]
harness = "claude-code"
model = "opus"             # Claude Code's alias for its newest Opus (Opus 5.5)
effort = "medium"

[team.skills.tester]       # also [team.skills.git-coder]
model = "sonnet"
effort = "low"

[commands]
# opencode = "/path/to/opencode"   # when it is not on PATH (e.g. an nvm install)
# antigravity = "/path/to/agy"
```

An Antigravity worker cannot ask for permission: a headless `agy` run denies every tool call
that needs it (shell commands included), and `--mode auto` is not available on every account.
Such a run is reported as failed, naming the denied calls. To let an `agy` coder build and test,
set `permission_mode = "bypassPermissions"` for that role, which runs `agy
--dangerously-skip-permissions`; otherwise keep the coder on its Claude Code fallback.

A project can override these in `.team/config.toml`, with the same `[team]` and
`[team.skills.<skill>]` tables. Harness, model and effort come as one unit from the first of:
project skill, user skill, the role's built-in default, project `[team]`, user `[team]`, then
`claude-code` + `opus` + `medium`. A table that sets a model or effort without a harness runs on
`claude-code` (never on another layer's Antigravity or OpenCode); what it leaves unset comes from
the next layer on the same harness. So `[team] model = "opus"` leaves the coder on Antigravity
with its `agy` model, and `[team.skills.coder] model = "sonnet"` runs the coder on Claude Code.
When a role's harness is not installed, the worker runs on that role's fallback as a whole (the
coder: Claude Code + `sonnet` + `medium`), and its chat says so. Permission mode and tools merge
field by field, the most specific first. A bad setting (unknown harness or effort, a
Coordinator off Claude Code) is replaced and shown as a warning in **Settings**; an effort the
harness lacks maps to the nearest lower one, and a harness with no effort setting (OpenCode)
runs without one. `jg-team-dev worker … --model <m> --effort <e>` overrides one worker.

## Development

```sh
cargo build                       # jg-team-dev, jg-team-dev-hub, jg-team-dev-desktop in target/debug
cargo test --workspace
./target/debug/jg-team-dev-hub    # run the hub in the foreground (RUST_LOG=jgt_hub=debug)
./target/debug/jg-team-dev start ~/my-app  # start (or wake) the Coordinator for a folder
./target/debug/jg-team-dev worker coder --task "fix average" --worktree fix-average --parent <id> "Remove the - 1"
./target/debug/jg-team-dev task list --workspace ~/my-app  # the folder's Board
./target/debug/jg-team-dev task export --workspace ~/my-app  # write it to docs/board.md
./target/debug/jg-team-dev settings        # hooks for `claude --settings`
./target/debug/jg-team-dev settings --mcp  # MCP server for `claude --mcp-config`
```

Use `JGT_RUNTIME_DIR` (keep it short: unix socket paths are limited to ~100 bytes),
`JGT_DATA_DIR` and `JGT_CONFIG_DIR` to run an isolated hub next to the installed one.

| Path | What |
|---|---|
| `crates/jgt-core` | shared types, hub API client, config, paths, launcher |
| `crates/jgt-hub` | the daemon: SQLite, HTTP API over a unix socket, liveness monitor, notifications, tray |
| `crates/jgt-cli` | `jg-team-dev`: MCP server, Claude Code hooks, `start`, settings generator, CLI |
| `skills/` | role skills, embedded in the binary and loaded as the `jg-team-dev` plugin |
| `crates/jgt-desktop` | `jg-team-dev-desktop`: the iced app |
| `packaging/` | PKGBUILD, systemd user unit, desktop entry, icon |

Files at runtime: database `~/.local/share/jg-team-dev/jg-team-dev.db`, socket
`$XDG_RUNTIME_DIR/jg-team-dev/hub.sock`, config `~/.config/jg-team-dev/config.toml`.

## License

MIT
