# jg-team-dev

A desktop chat with a **Coordinator**: a Claude Code session that listens to you and runs a team
of worker sessions (coder, reviewer, git-coder). Each worker starts with a
clean context and one skill, reports progress in its own chat box, and exits when its task is done.

## In short

jg-team-dev is a desktop app for running a small team of AI coding agents on your own projects.
You chat with a Coordinator, which breaks the work into tasks on a Kanban board.
The Coordinator writes the brief into the task, then coder, reviewer and git-coder take each task in turn, each in its own chat.
Every task gets its own branch and worktree; the coder runs the tests and the reviewer checks the full diff.
You approve each git step (commit, push, pull request, merge) unless you switch it to automatic.
The app, its hub and its data run on your machine; the agents use Claude Code or Antigravity.

```
you ⇄ jg-team-dev app ⇄ jg-team-dev-hub (SQLite, unix socket) ⇄ MCP + hooks ⇄ Coordinator ─ claude -p ─▶ workers
```

The messaging layer consists of a background hub daemon (`jg-team-dev-hub`), an MCP server, session hooks, and an iced desktop application (`jg-team-dev-desktop`). The system operates with its own unix socket, SQLite database, and configuration, and passes its hooks to sessions with `--settings` instead of modifying `~/.claude/settings.json`.

## Features

- **Coordinator chat**: `jg-team-dev start [folder]` (or **New team** in the app) marks the folder
  trusted and starts a background Coordinator with jg-team-dev's hooks, MCP server and skills. It
  listens for 50 minutes (`[timeouts] listen_secs`), then its process stops; your next message
  wakes it with its context intact.

- **Workers and skills**: the Coordinator runs `jg-team-dev worker <skill> --task … --brief-file …`
  (coder, reviewer, git-coder; there are no separate verifier or tester workers: the coder
  runs the build and tests, and the reviewer is the only gate). It returns at once; a detached process runs
  `claude -p "/jg-team-dev:<skill> <brief>"` (or Antigravity / OpenCode as configured) and relays
  the result JSON to the Coordinator through the hub, waking it if needed. Workers get their own
  chats under their Coordinator; messaging a finished worker resumes it for one reply, and the
  Coordinator gets a summary. `jg-team-dev workers` lists them (the Coordinator keeps at most 3 running).

- **Git flow**: coders work in `.team/worktrees/<name>` on branch `jgt/<name>`
  (`jg-team-dev worker coder --worktree <name>`; the reviewer points at the same worktree). In a
  git repository a coder without a worktree is refused unless the Coordinator passes `--main-worktree`
  after warning you. `.team/` is kept out of `git status` through the repository's local exclude
  file. The git-coder commits each task on its own branch, pushes it, opens a pull request and merges
  it (using `gh pr merge <n> --merge` without `--delete-branch`, deleting the remote branch with
  `git push origin --delete`, removing the worktree, and deleting the local branch with `git branch -d`),
  each step only after a review, a test and your OK (or its auto-switch); it never commits, merges or
  pushes on `main`, and removes the worktree once the pull request is merged. Inspect the live policy
  at any time with `jg-team-dev git-policy`.

- **Runs**: each request with workers is a *run*. `jg-team-dev run start` writes
  `.team/runs/<run-id>/request.md`; workers append to `events.jsonl` and save their result in
  `worker-results/`; `jg-team-dev run finish` writes `final-report.md` and archives the run's worker
  chats. `jg-team-dev run list` shows a project's runs.

- **Team panel**: a Coordinator's chat has a **Team** panel showing each worker's state, time and
  cost; click one to open its chat. Model, effort, permission mode and tools can be set per skill and
  per project (see [Configuration](#configuration) below).

- **Topic change and reset**: on a topic change the Coordinator asks, writes
  `.team/tasks/task-<date>-<slug>.md`, and runs `jg-team-dev reset --summary <file>`: a fresh
  Coordinator takes over the folder and the old chat is archived, read-only.

- **Kanban Board**: each workspace (a Coordinator's folder) has a Board of tasks in the hub
  database, `JGT-1`, `JGT-2`, … per workspace (keys are never reused), in ten columns:
  Backlog, On Queue, Working, Testing, Review, To commit, To push, To open PR, To merge, Done.
  Between Review and Done sits the fixed git line (To commit → To push → To open PR → To merge →
  Done), where each column represents the next git step awaiting the user's OK (To merge = merge
  the pull request). When the Coordinator moves a card to `done` from Review or a git column, the
  hub routes it to the first later column whose auto-switch is OFF, or to Done if all are ON. The hub
  returns the ON steps passed along the way (`git_steps`: `commit`, `push`, `open_pr`, `merge_pr`) and
  the task's branch name (`branch`), which the Coordinator hands to the git-coder. In a git
  repository with no remote the card stops in To push, and without `gh` on PATH it stops in To open
  PR, whatever the switches say; the task log says why. Dragging a card forward along the line counts
  as the user's OK for every step it passes, and the hub notifies the Coordinator to run them. Leaving
  a git column whose switch is OFF requires a reason quoting the user's OK. The team can never move a
  card backward from a git column. Git columns do not count toward the WIP limit. Every move, edit,
  result and comment goes to the task's activity log with who made it. The hub enforces the gates:
  workers only read the Board; agents never edit the title, description or priority; the Coordinator
  creates tasks in Backlog and queues only subtasks of a task you queued; at most `wip_limit` tasks
  are in progress. The flow is: Coordinator writes the brief, plan and acceptance criteria into the task Details →
  coder implements and tests (stays in Working until tests are green) → one reviewer pass checks only critical things
  across the full diff. If clean, the card moves to Done and the hub routes it along the git line. If changes are requested,
  the card stays in Review for the user; when the user moves it back to Working (or asks for a fix), a fresh coder fixes
  the findings, runs tests, and a new full review runs. Medium and low findings no longer exist. You may move any task anywhere;
  a move that skips a gate is logged as "moved by you".
  `jg-team-dev task` (`list`, `show`, `create`, `edit`, `details`, `move`, `result`, `comment`,
  `delete`, `log`, `export`) drives it from a terminal or a session.

- **Board view**: a Coordinator's chat header has two tabs, **Chat** (the default) and **Board**.
  The Board shows the workspace's ten columns with their counts, scrolling sideways in a narrow
  window; Done shows the 20 newest tasks, with "Show all". "+" in Backlog and On Queue adds a task
  (a title is enough). A card shows its key, title, priority, the worker on it (`↳ reviewer`),
  the review decision and *Waiting for you* or *Waiting: usage limit*; move it with ← → or
  "Move to…". Click a card for its panel: edit the title, description and priority; read the
  Coordinator's Details, the test and review results (critical findings first) and the activity
  log; open the run folder or a worker's chat; delete it. Changes from the team or a terminal show
  up live.

- **Team on the board**: moving a task to On Queue is the request. The hub queues
  `[jg-team-dev] Task JGT-n "…" is on the queue` for the folder's current Coordinator, and the
  app or the CLI that moved it wakes a sleeping Coordinator (`team::wake`, shared with worker
  results; the hub never starts agents). With no Coordinator running, the task's log says so and
  the app offers "Start team". The Coordinator works the task with the MCP tools `task_list`,
  `task_get`, `task_update`, `task_move`, `task_create` and `task_log`, following the "Board"
  section of its skill: it moves the card before announcing each step and follows the one-review-pass
  rule. Workers only read (`task_get`). `jg-team-dev run start --task-id JGT-n` names the run after
  the key and links it to the task; `jg-team-dev worker … --task-id JGT-n` works in
  `.team/worktrees/jgt-n` (branch `jgt/jgt-n`) in the task's run, and the card shows the worker
  on it. The reviewer checks only critical findings (severity `blocking` or `high`, category `behavior`,
  `security`, `secret`, or `patterns`).

- **Board interactions and export**: drag a card to another column or position (Esc cancels; ← →
  and "Move to…" stay). Comment on a task from its panel or `jg-team-dev task comment`: the
  comment reaches the Coordinator like a chat message and wakes it. A task reaching Done,
  entering a git column waiting for your OK, or getting *Waiting for you*, posts a notification in
  the Coordinator's chat, which also shows on the desktop. Done tasks are archived (kept, hidden;
  `task list --all` shows them) 7 days after they got there; moving one brings it back. Export the
  Board as Markdown with the Board's Export button or `jg-team-dev task export [--out FILE] [--all]`
  (default `docs/board.md`).

- **Team efficiency and review gate**: the coder runs the build and tests itself and reports them
  (`tests` field); while tests fail (or work is incomplete), the task stays in Working and the
  Coordinator starts a fresh coder to make them green, recording no test result. Once tests are green,
  the Coordinator records it as the task's test result (`pass`) and moves the card to Review. The
  **reviewer** is the only gate: it does not re-run tests, it checks only critical things (`behavior`,
  `security`, `secret`, `patterns` when documented). One review pass per task, reviewing the full
  worktree diff (`git diff`, all coders). If the reviewer requests changes, the card stays in Review
  waiting for the user.
  A finished worker's result reaches the Coordinator as a short digest (at most 10 lines: status,
  finding counts, notes) with the path of the full result in the run, which also shows in the
  worker's chat. A worker stopped by the usage limit sets *Waiting: usage limit* on its task with the
  reset time; the Coordinator starts no worker meanwhile, and shortly after the reset the hub clears
  it and wakes the Coordinator, which resumes the task with a fresh worker from the saved resume
  brief. `jg-team-dev worker … --model <m> --effort <e>` raises one worker for a hard task without
  changing the config.

- **IDE tab**: built-in read-only viewer in the desktop app sidebar (and from task panels) for
  inspecting workspace files and git worktree changes across `main` and active tasks, featuring
  syntax highlighting, diff toggles, and safety guards (see [IDE tab](#ide-tab) below).

## Configuration

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

[team.skills.coder]
harness = "antigravity"           # Google Antigravity (`agy`)
model = "gemini-3.8-flash-medium" # Gemini 3.8 Flash, as `agy models` names it
effort = "medium"
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
Settings save to `.team/agents.json` when a team starts. To change them live, edit them in **Settings** and click **Apply to this team**, or run `jg-team-dev agents set <role> [--harness h] [--model m] [--effort e]`; new workers use the changes immediately (the Coordinator row applies after a reset).

## IDE tab

The desktop app includes an **IDE** module (in the sidebar below Board, or opened via the **Open in IDE** button on a task panel) for viewing project files and worktree changes:

- **Read-only viewer**: displays highlighted file contents with line numbers. There is no editing, saving, or code execution.
- **Source selector**: choose between `main` (the committed `main` branch from git, reflecting committed state) and any task worktree under `.team/worktrees/` (showing working tree files, including uncommitted coder changes).
- **Picker on the right**: directory tree on the right side of the view, sorted folders-first and lazily expanded.
- **M/A/D marks and "Changed only"**: worktree files display badges for Modified (`M`), Added (`A`), Deleted (`D`), and Renamed (`R`) changes relative to the merge base with `main`. A **Changed only** checkbox filters the tree to only show files with changes.
- **File | Diff toggle**: for changed files, toggle between the full file text and the unified diff view with colored additions and deletions. Deleted files show the diff view directly.
- **Limits and safety**:
  - File size cap: 1 MB (`File too large to show`).
  - Binary files (NUL byte in the first 8 KB or invalid UTF-8) are refused (`Binary file`).
  - Directory listings are capped at 20,000 entries (`Listing truncated`).
  - Diff output capped at 1 MB / 20,000 lines.
  - Automatically skipped folders: `.git`, `target`, `node_modules`, `.team`.
  - Secret files (`.env`, `.env.*` except `.env.example`, `.env.sample`, `.env.template`; `*.pem`, `*.key`, `id_rsa*`, `credentials*`, `*.p12`, `.npmrc`, `.pypirc`) are listed with a lock icon, and their contents and diffs are never read or shown (`Secret file: not shown`).
  - Symlinks are not followed (`Symlink: not followed`), and path traversal (`..` components or paths escaping the root) is rejected.
- **Refresh button**: reloads sources and file listings manually on demand; there is no auto-refresh or background file watching.

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
