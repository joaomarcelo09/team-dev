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

Team sessions use `[team]` in `config.toml`:

```toml
[team]
model = "opus"            # claude --model
effort = "medium"         # claude --effort
permission_mode = "auto"  # only prompts auto mode can't settle reach the app
```

## Development

```sh
cargo build                       # jg-team-dev, jg-team-dev-hub, jg-team-dev-desktop in target/debug
cargo test --workspace
./target/debug/jg-team-dev-hub    # run the hub in the foreground (RUST_LOG=jgt_hub=debug)
./target/debug/jg-team-dev start ~/my-app  # start (or wake) the Coordinator for a folder
./target/debug/jg-team-dev worker coder --task "fix average" --worktree fix-average --parent <id> "Remove the - 1"
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
