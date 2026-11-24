# Dark Factory end-to-end run on a toy project (JGT-104)

`jg-factory e2e --owner <github-owner>` runs the whole factory flow once on the toy notes
project in `e2e/toy/`: spec intake, plan, coder, CI fix cycle, review and merge into the integration
branch, with the audit log and the kill switch.

## How a run works

1. Intake freezes `e2e/toy/SPEC.md`; the plan puts one task on the Board.
2. The runner creates a private GitHub repository (`jgf-e2e-toy-<timestamp>`) through the broker,
   pushes `main` and `develop` from the fixture, and refuses to start when the fixture has no
   `.github/workflows/*.yml` at its root.
3. The coder (`claude -p`) works on `jgt/e2e-toy-<n>` inside one `jg-sandbox` world shared with the
   reviewer. The per-run lab contains sibling `work/` and `scratch/` directories; only the task
   work tree, required read-only runtime paths, and the single Claude credentials file are exposed.
   The work tree is writable except for read-only `.git`, and the scratch HOME contains only Claude
   credentials. Direct networking is disabled; the host egress
   proxy allows only `api.anthropic.com:443` for Claude API traffic. The world and scratch HOME are
   destroyed when the run ends.
4. The runner commits as `Dark Factory` with a `Co-authored-by` trailer for the operator (read from the
   global git config; `--co-author` and `--no-co-author` override it), pushes the task branch and opens a
   PR into `develop`.
5. CI is read through the broker. Red CI starts a fresh coder with the failure log path, up to
   `MAX_FIX_CYCLES`. When no CI run exists for the pushed commit after a few minutes the run fails.
6. A read-only reviewer must approve; the PR is then merged into the integration branch, never `main`.

Every step is appended to the hash-chained audit log under `<state-dir>/labs/<lab>/audit.jsonl`.
A run ends in exactly one of `delivered`, `failed`, `budget_exhausted` or `killed`.

## Commands

- `jg-factory e2e --owner <owner> [--repo <name>] [--state-dir <dir>] [--max-pushes-per-day <n>]`
- `jg-factory e2e --dry-run`: fake backend, fake coder, fake reviewer; no network.
- `jg-factory e2e kill --state-dir <dir>`: creates `<state-dir>/e2e/KILL`; the run stops between steps and
  while a child runs, keeps its work tree, Board and audit log, and ends `killed`.
- `jg-factory e2e verify --state-dir <dir>`: checks the hash chain, the step order, that no token-like
  string is in the log, and that the run has exactly one terminal outcome.

The lab token is read from `<state-dir>/secrets/lab/GITHUB_TOKEN` (a `gh auth token` with the `workflow`
scope, because the fixture pushes a workflow file). The runner's push cap defaults to 1000 per day.

## Real run report (2026-11-24)

Six real runs were needed; each one found a real defect that is now fixed and covered by tests:

| Run | Result | Cause and fix |
|---|---|---|
| 1 | failed | token file ended with a newline (invalid header); token is now trimmed |
| 2 | failed | `claude` could not resolve DNS inside `bwrap`; `/run/systemd/resolve` is bound read-only, and failed child runs now keep a scrubbed stdout/stderr tail |
| 3 | killed | toy CI never started (workflows were not at the repo root); stopped with `e2e kill`, work tree and log kept; a root workflow and no-run detection were added |
| 4 | failed | token lacked the `workflow` scope; refreshed by the operator |
| 5 | failed | the broker's daily push cap (10) was used up by earlier runs; the runner cap is now configurable, default 1000 |
| 6 | **delivered** | repo `jgf-e2e-toy-1795551902737`: task branch `jgt/e2e-toy-5`, PR opened into `develop`, CI green after 11 pending polls, review approved, PR merged, `e2e verify`: ok (51 events, outcome delivered) |

Commits of run 6 are authored by `Dark Factory` with the operator as co-author.
The kill switch was verified live in run 3 and by tests (kill before start, mid-coder, during CI wait).
