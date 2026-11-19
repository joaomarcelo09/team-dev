# The usage limit

- **The usage limit.** `[jg-team-dev] Worker … stopped by the usage limit; resets 17:00` is not
  a failure: the worker's harness ran out of its usage window. The hub marks its task *Waiting:
  usage limit*. **Start no worker for any task until it resets** (a worker on another harness,
  for example an Antigravity coder, is not bound by Claude Code's limit). Tell the user in one
  line and end your turn. About two minutes after the reset the hub wakes you with
  `[jg-team-dev] Usage limit reset: resume JGT-n`: start a **fresh** worker of the same skill in
  the same worktree, with the resume brief the message names (`--brief-file <that path>`): it
  holds the original brief, the worktree's `git status --short` and `git diff --stat`, and the
  line "Resume: … do not restart."
