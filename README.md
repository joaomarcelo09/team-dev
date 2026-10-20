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

Next is Phase 2: workers (`claude -p "/jg-team-dev:<skill> <brief>"`).

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
