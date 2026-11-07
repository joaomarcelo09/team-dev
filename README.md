# jg-team-dev

A desktop chat with a **Coordinator**: a Claude Code session that listens to you and runs a team
of worker sessions (planner, coder, reviewer, git-coder). Each worker starts with a
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
  (planner, coder, reviewer, git-coder). It returns at once; a detached process runs
  `claude -p "/jg-team-dev:<skill> <brief>"` and relays the result JSON to the Coordinator
  through the hub, waking it if needed. Workers get their own chats under their Coordinator;
  messaging a finished worker resumes it for one reply, and the Coordinator gets a summary.
  `jg-team-dev workers` lists them (the Coordinator keeps at most 3 running).

- **Phase 3** (git and memory): coders work in `.team/worktrees/<name>` on branch `jgt/<name>`
  (`jg-team-dev worker coder --worktree <name>`; the reviewer points at the same worktree). In a
  git repository a coder without a worktree is refused unless the Coordinator
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
  `wip_limit` tasks are in progress. The flow is coder (stays in Working until tests are green;
  no rounds used) → reviewer (full diff) → one fix round if critical/high findings (fresh coder
  fixes them and re-runs tests) → reviewer (fix diff only) → Orchestrator review. A task whose first
  review is clean goes Review → Done. Otherwise it gets one automatic fix round (back to Working;
  rounds are consumed only by review findings) and a second review, then waits in **Orchestrator
  review**, where only you decide: done, one more fix (one more coder and reviewer, then back to
  Orchestrator review), take over, or stop. Findings that are not critical or high go to a
  follow-up task in Backlog and never start a fix round. You
  may move any task anywhere; a move that skips a gate is logged as "moved by you". Reopening a
  Done task resets its rounds. `jg-team-dev task` (`list`, `show`, `create`, `edit`, `details`, `move`,
  `result`, `decide`, `comment`, `delete`, `log`) drives it from a terminal or a session.

- **Kanban (K-2, Board view)**: a Coordinator's chat header has two tabs, **Chat** (the default)
  and **Board**. The Board shows the workspace's seven columns with their counts, scrolling
  sideways in a narrow window; Done shows the 20 newest tasks, with "Show all". "+" in Backlog
  and On Queue adds a task (a title is enough). A card shows its key, title, priority, the worker
  on it (`↳ reviewer`), the review (`round 1/2`) and *Waiting for you* or *Waiting: usage
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
  on it. The reviewer tags each finding by severity, with a category (security,
  secret, performance, patterns, docs, behavior); only critical and high findings start a fix round.

- **Kanban (K-4, polish)**: drag a card to another column or position (Esc cancels; ← →
  and "Move to…" stay). Comment on a task from its panel or `jg-team-dev task comment`: the
  comment reaches the Coordinator like a chat message and wakes it. A task reaching Done, or
  getting *Waiting for you*, posts a notification in the Coordinator's chat, which also shows on
  the desktop. Done tasks are archived (kept, hidden; `task list --all` shows them) 7 days after
  they got there; moving one brings it back. Export the Board as Markdown with the Board's
  Export button or `jg-team-dev task export [--out FILE] [--all]` (default `docs/board.md`).

- **Team efficiency**: the coder runs the build and tests itself and reports them (`tests` field);
  while tests fail (or work is incomplete), the task stays in Working and the Coordinator starts a
  fresh coder to make them green, recording no test result and using no fix round. Once tests are
  green, the Coordinator records it as the task's test result (`pass`) and moves the card to Review.
  The **reviewer** is the only gate: it does not re-run tests, it reviews architecture, code, test
  results and how the change affects the app and the integration between services. One reviewer per
  task: a big task (frontend + backend) may have several coders sharing one worktree, but a single
  reviewer reviews the whole task at the end. The FIRST review always covers the full worktree diff
  (`git diff`, all coders); fix rounds (consumed only by review findings) check only the new diff:
  `jg-team-dev snapshot --task JGT-n` saves what a review checked as a git tree under
  `refs/jgt/<key>/checked-<round>` (no commit, the branch and index stay untouched), and the second
  review uses `jg-team-dev snapshot --task JGT-n --diff` to review the diff since then. A finished worker's result reaches the Coordinator as a short digest (at most 10 lines:
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

[team.skills.reviewer]
harness = "claude-code"
model = "opus"             # Claude Code's alias for its newest Opus (Opus 5.5)
effort = "medium"

[team.skills.git-coder]
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

The Coordinator compacts its context at `compact_window = 120000` tokens unless set under
`[team.skills.coordinator]` (`0` leaves it to Claude Code): it lives for days, and every step
re-reads its whole context. Any Claude Code role takes `compact_window` (100000 to 1000000).

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
Settings apply when a team starts (saved in `.team/agents.json` when the Coordinator starts);
editing `config.toml` afterwards does not change a running team.

## Installation

### From AUR (Arch Linux)

Once published, the AUR package can be installed using an AUR helper:

```sh
yay -S jg-team-dev
# or
paru -S jg-team-dev
```

### From Source (makepkg)

To build and install the package locally from this repository:

```sh
cd packaging && makepkg -si
```

### Runtime Requirements

- **Claude Code** (`claude-code`): required for agent sessions.
- **Vulkan driver** (`vulkan-icd-loader`): optional, enables GPU-accelerated rendering.

### First Start

1. Enable and start the background hub service:
   ```sh
   systemctl --user enable --now jg-team-dev-hub
   ```
2. Open **JG Team Dev** from your application launcher or run `jg-team-dev-desktop`.

For packaging and publishing instructions, see the [AUR Maintainer Guide](docs/AUR.md).

## Voice call mode (optional)

`jg-team-dev-desktop` can be built with voice call mode enabled to converse directly with the Coordinator using your microphone and speakers.

### Requirements

- **CPU**: x86-64 processor with AVX2 support (Intel Haswell 2013+ / AMD Excavator 2015+) for real-time STT
- **Build dependencies**: `cmake`, `clang`, `alsa-lib`
- **Runtime speech synthesis**: Piper (`pip install piper-tts` or distro package `piper-tts`)

### Building with Voice Support

```sh
cargo build --release --locked --workspace --features jgt-desktop/voice
```

### Setup and Usage

1. Open **Settings** in the desktop application.
2. In the **Voice call** section, switch **Voice mode** to **On**.
3. Click **Download models** to fetch the STT and TTS models (~602 MB from Hugging Face: Whisper `medium.en-q5_0` and Piper `en_US-lessac-medium`).
4. Select a Coordinator session in the sidebar and click the **Call** button in the header.
5. Use **Mute** to toggle your microphone or **End** to hang up.

### Echo Modes

- **Speakers (`half_duplex`)**: Microphone is paused while the Coordinator speaks to eliminate acoustic feedback.
- **Headset (`headset`, recommended)**: Full duplex mode. Continuous user speech (~250 ms) interrupts the Coordinator (barge-in).
- **PipeWire echo-cancel (`pipewire_aec`)**: Full duplex mode utilizing PipeWire acoustic echo cancellation.

### Configuration

Voice settings persist in `~/.config/jg-team-dev/config.toml`:

```toml
[voice]
enabled = true
stt_model = "medium.en-q5_0"
tts_command = "piper"
tts_voice = "en_US-lessac-medium"
speak_updates = true
echo = "headset"           # half_duplex | headset | pipewire_aec
# input_device = "..."     # optional audio capture device
# output_device = "..."    # optional playback device
# threads = 4              # optional thread count
```

> [!NOTE]
> Other STT models such as `small.en-q5_1` (faster transcription, smaller download), `small.en`, or `large-v3-turbo-q5_0` (~547 MiB download, more accurate, larger/slower on CPU, language forced to English) can be configured via `voice.stt_model`.

### Privacy & Language

- **English only**: Whisper `medium.en-q5_0` and Piper `en_US-lessac-medium` are configured for English speech.
- **100% local**: All audio transcription and voice synthesis occur locally on your device. Only the initial model download reaches out to huggingface.co.

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
