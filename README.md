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
  repository's local exclude file. The git-coder commits each task on its own branch, pushes it,
  opens a pull request and merges it (using `gh pr merge <n> --merge` without `--delete-branch`,
  deleting the remote branch with `git push origin --delete`, removing the worktree, and deleting
  the local branch with `git branch -d`), each step only after a review, a test and your OK (or its
  auto-switch); it never commits, merges or pushes on `main`, and removes the worktree once the
  pull request is merged. On a topic change the Coordinator asks,
  writes `.team/tasks/task-<date>-<slug>.md`, and runs `jg-team-dev reset --summary <file>`: a
  fresh Coordinator takes over the folder and the old chat is archived, read-only.

- **Phase 4** (polish): each request with workers is a *run*. `jg-team-dev run start` writes
  `.team/runs/<run-id>/request.md`; workers append to `events.jsonl` and save their result in
  `worker-results/`; `jg-team-dev run finish` writes `final-report.md` and archives the run's
  worker chats. `jg-team-dev run list` shows a project's runs. A Coordinator's chat has a **Team**
  panel: each worker's state, time and cost; click one to open its chat. Model, effort,
  permission mode and tools can be set per skill and per project (below).

- **Kanban (K-1, board data)**: each workspace (a Coordinator's folder) has a Board of tasks in
  the hub database, `JGT-1`, `JGT-2`, … per workspace (keys are never reused), in ten columns:
  Backlog, On Queue, Working, Testing, Review, To commit, To push, To open PR, To merge, Done.
  Between Review and Done sits the fixed git line (To commit → To push → To open PR → To merge →
  Done), where each column represents the next git step awaiting the user's OK (To merge = merge
  the pull request). When the Coordinator moves a card to `done`
  from Review or a git column, the hub routes it to the first later column whose auto-switch is OFF,
  or to Done if all are ON. The hub returns the ON steps passed along the way (`git_steps`: `commit`,
  `push`, `open_pr`, `merge_pr`) and the task's branch name (`branch`), which the Coordinator
  hands to the git-coder. In a git repository with no remote the card stops in To push, and
  without `gh` on PATH it stops in To open PR, whatever the switches say; the task log says why. Dragging a card forward along the line counts as the user's
  OK for every step it passes, and the hub notifies the Coordinator to run them. Leaving a git column
  whose switch is OFF requires a reason quoting the user's OK. The team can never move a card
  backward from a git column. Git columns do not count toward the WIP limit. Every move, edit, result
  and comment goes to the task's activity log with who made it. The hub enforces the gates: workers
  only read the Board; agents never edit the title, description or priority; the Coordinator creates
  tasks in Backlog and queues only subtasks of a task you queued; at most `wip_limit` tasks are in
  progress. The flow is coder (stays in Working until tests are green; no rounds used) → reviewer
  (full diff) → one fix round if blocking/high findings (fresh coder fixes them and re-runs tests) →
  reviewer (fix diff only) → routing along the git line to Done. Review → Done is accepted for an
  approved review at any round, and for a review with changes only when no fix round is left. The hub
  then files the leftover findings as ONE follow-up in Backlog (parent = the task) and sets its
  priority itself = highest severity (blocking/high → high; medium or unlabelled → normal; only low →
  low; no findings → normal). The task never waits for the user (a red test at the last round needs a
  user move). You may move any task anywhere; a move that skips a gate is logged as "moved by you".
  Reopening a task from a git column or Done (moving to On Queue) resets its rounds. `jg-team-dev task`
  (`list`, `show`, `create`, `edit`, `details`, `move`, `result`, `comment`, `delete`, `log`) drives
  it from a terminal or a session.

- **Kanban (K-2, Board view)**: a Coordinator's chat header has two tabs, **Chat** (the default)
  and **Board**. The Board shows the workspace's ten columns with their counts, scrolling
  sideways in a narrow window; Done shows the 20 newest tasks, with "Show all". "+" in Backlog
  and On Queue adds a task (a title is enough). A card shows its key, title, priority, the worker
  on it (`↳ reviewer`), the review (`round 1/2`) and *Waiting for you* or *Waiting: usage
  limit*; move it with ← → or
  "Move to…". Click a card for its panel: edit the title, description and priority; read the
  Coordinator's Details, the test and review results (critical findings first) and the activity
  log; open the run folder or a worker's chat; delete it. Changes from the team or a terminal show
  up live.

- **Kanban (K-3, team on the board)**: moving a task to On Queue is the request. The hub queues
  `[jg-team-dev] Task JGT-n "…" is on the queue` for the folder's current Coordinator, and the
  app or the CLI that moved it wakes a sleeping Coordinator (`team::wake`, shared with worker
  results; the hub never starts agents). With no Coordinator running, the task's log says so and
  the app offers "Start team". The Coordinator works the task with the MCP tools `task_list`,
  `task_get`, `task_update`, `task_move`, `task_create` and `task_log`, following
  the "Board" section of its skill: it moves the card before announcing each step and keeps the
  one-fix-round rule. Workers only read (`task_get`). `jg-team-dev run start --task-id JGT-n` names the
  run after the key and links it to the task; `jg-team-dev worker … --task-id JGT-n` works in
  `.team/worktrees/jgt-n` (branch `jgt/jgt-n`) in the task's run, and the card shows the worker
  on it. The reviewer tags each finding by severity (`blocking`, `high`, `medium`, `low`), with a category
  (security, secret, performance, patterns, docs, behavior); only blocking and high findings start a fix round.

- **Kanban (K-4, polish)**: drag a card to another column or position (Esc cancels; ← →
  and "Move to…" stay). Comment on a task from its panel or `jg-team-dev task comment`: the
  comment reaches the Coordinator like a chat message and wakes it. A task reaching Done,
  entering a git column waiting for your OK, or getting *Waiting for you*, posts a notification in
  the Coordinator's chat, which also shows on the desktop. Done tasks are archived (kept, hidden;
  `task list --all` shows them) 7 days after they got there; moving one brings it back. Export the
  Board as Markdown with the Board's Export button or `jg-team-dev task export [--out FILE] [--all]`
  (default `docs/board.md`).

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

[git]
auto_commit = false        # commit a reviewed task on its own branch without asking (default: false)
branch_pattern = ""        # task branch name; {key} {slug} {title}; empty = "jgt/{key}"
auto_push = false          # push the task branch to the remote without asking (default: false)
auto_open_pr = false       # open a pull request for the pushed branch (default: false)
auto_merge_pr = false      # merge the pull request once its checks pass (default: false)

[commands]
# opencode = "/path/to/opencode"   # when it is not on PATH (e.g. an nvm install)
# antigravity = "/path/to/agy"
```

The `[git]` section configures git autonomy: the steps the team takes on its own once the
reviewer approved a task. They run in a fixed order, and each one that is `false` (the default)
makes the card wait in its column for your OK:

1. `auto_commit`: at the task's first commit the team creates the task's branch and commits only
   there. `branch_pattern` names it: `{key}` is the task key in lowercase (`jgt-31`), `{slug}` a
   slug of the title (a-z, 0-9 and `-`, at most 40 characters) and `{title}` the title with
   spaces turned into `-`. Empty, or a pattern that ends up empty or as `main`, `master` or
   `HEAD`, means `jgt/{key}`.
2. `auto_push`: push that branch to the remote (a plain push).
3. `auto_open_pr`: open a pull request for it with `gh`, against the repository's default branch.
4. `auto_merge_pr`: merge the pull request once its checks pass (using `gh pr merge <n> --merge`
   without `--delete-branch`, deleting the remote branch with `git push origin --delete`, removing
   the worktree, and deleting the local branch with `git branch -d`), and fast-forward your local
   default branch when the main worktree is clean and on it.

The team never commits, merges or pushes on `main`, and force-push is never allowed, under any
setting. Opening and merging pull requests needs a git remote and the GitHub CLI (`gh`), logged
in. Inspect the live policy at any time with `jg-team-dev git-policy`: it prints the four
switches, the branch pattern and the "never" line.

Older configs migrate on load: `auto_merge = true` becomes `auto_open_pr = true` (which now talks
to GitHub) unless `auto_open_pr` is already set, and the old main-branch switch is read and dropped; the
next save writes only the new keys. On the Board, cards that were in the old To merge column (committed,
not merged) move to To push. Cards that were already in To push had been merged into your local
`main`: the team no longer pushes `main`, so push it yourself and move them to Done.

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
