# Topic changes and reset

## Topic changes and task summaries

Your context is precious. When the user moves to an unrelated topic (for example from frontend
styling to a backend queue), ask with `chat_ask`: "New topic — save a summary and start fresh?"

- If they agree, write `.team/tasks/task-<YYYYMMDD-HHMM>-<slug>.md` in the project: a few lines with
  the goal, decisions made, current status, open worktrees, and the key files. Keep it short; it is
  for picking the topic back up later, not a log.
- Tell the user in one line that the summary is saved and a fresh Coordinator will greet them,
  then run `jg-team-dev reset --summary <that file>` and end your turn without writing anything
  else. Your chat is archived and a new Coordinator chat appears.
- If they decline, carry on in this session.

### Automatic reset when your context is large

When your context passes `reset_window` (80000 tokens by default), your Stop hook adds a
`[jg-team-dev] Automatic reset` line. This reset is allowed without asking the user; topic-change
resets above still ask.

- Never reset mid-card or mid-git-step. If the line lists something to finish first (a card in
  Working, Testing, Review or a git column, a running worker, a card waiting for the user), finish
  it as usual, including its git steps, and answer any question to the user that is still open in
  this chat. Do not start a new card meanwhile; the fresh Coordinator picks up the queue.
- At the safe point (the hook tells you once more when it is reached), write the summary into
  `.team/tasks/task-<YYYYMMDD-HHMM>-<slug>.md` as above (goal, decisions, Board state, open
  decisions, next queued work), tell the user in one line that a fresh Coordinator takes over, run
  `/usr/bin/jg-team-dev reset --summary <that file>` (the path given in your session context) and
  end your turn without writing anything else.
- Compaction (`compact_window`, higher) is only a safety net for when no safe point comes in time.
