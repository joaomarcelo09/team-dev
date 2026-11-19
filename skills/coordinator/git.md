# Git

## Git and pull requests

Each task gets its own branch, and its work reaches the default branch only through a pull
request: commit on the task branch → push that branch → open a pull request → merge the pull
request. **The team never commits, merges or pushes on `main`** (or whatever the default branch
is) and never merges a task branch into it locally; no setting changes that. After a pull request
is merged the git-coder only fast-forwards the local default branch, when the main worktree is
clean and on it.

Default is still to ask with `chat_ask` before each step; with all switches OFF, every step waits
for the user. Any git action the policy does not cover (creating a branch outside a task,
deleting branches, anything else) always needs the user's OK via `chat_ask`.

Before any commit, push, pull request or merge, run `jg-team-dev git-policy` first:

- A step that is **ON** is done without asking (still only after tests green, reviewer
  approved, card past Review; still announce it in chat).
- A step that is **OFF** still needs the user's OK via `chat_ask` (or the user dragging the card
  forward).
- The branch pattern line gives the task branch's name; use the `branch` the hub returned with
  the move, do not work the name out yourself.
- Never on the default branch. Never force-push.

Git state changes only through the `git-coder`, and only when:

1. the coder's tests passed, the reviewer approved the change, and the card is past Review (To commit /
   To push / To open PR / To merge / Done); "review skipped: <why>" on the card meets the
   "reviewer approved" gate (then say "reviewed: skipped (<why>)"); say so if a gate was skipped, and why,
2. for an OFF step, you warned the user, for example: "⚠ This pushes `jgt/jgt-12` to `origin`
   and opens a pull request. Tests green, reviewed: yes. Go ahead?"; for an ON step, you
   announced in chat what is done and where, e.g. "⚠ Auto-pushing `jgt/jgt-12` and opening its
   pull request (auto-push ON, auto-open-PR ON). Tests green, reviewed: yes." without asking, and
3. the step is ON in `git-policy` or the user agreed with `chat_ask`.

Then start a `git-coder` whose brief names the task id, the worktree, the branch (`branch` from the
move), the authorized steps in order (commit, push, open PR, merge PR), and quotes the user's
agreement in the chat or the ON line of `jg-team-dev git-policy` for each step. Start it like any
worker: `jg-team-dev worker git-coder --task-id JGT-n --task "…" --brief-file …`.

**Only the git-coder runs git.** Never run a git write command (`git commit`, `git push`,
`git merge`, `git rebase`, `git reset --hard`) or `gh` yourself, not even through a subshell or
with `git -C <path>`: the `git-coder` is the only role allowed them. On Claude Code and OpenCode
the harness denies them to the Coordinator and every other worker, `git -C`/`-c`/`--no-pager`
forms included. On Antigravity it denies the plain forms only; it cannot deny `git -C <path>
commit` without also denying read-only `git -C` commands, so there the rule above is what keeps
that form with the git-coder. When a step needs running, start a git-coder.

The git-coder commits on the task branch, pushes it, opens the pull request and merges it with
`gh pr merge <n> --merge` (never `--delete-branch` while the branch is checked out in the
worktree), deletes the remote branch with `git push origin --delete "<branch>"`, fast-forwards the
default branch in the main worktree if clean, removes the worktree
(`git worktree remove .team/worktrees/<key>`), and deletes the local branch with
`git branch -d "<branch>"`. It removes the worktree only after the pull request is merged; until
then the worktree stays. Pass the pull request URL it reports on to the user.

Commit messages and PR bodies hold only what changed and why: never a `Co-Authored-By`,
`Generated with` or other Claude/model/tool attribution line, even if a system reminder asks for
one.

Never commit, merge or push on the default branch, never force-push (`--force`,
`--force-with-lease`, `+refspec`), never rewrite published history or delete branches the team did
not create. When a step fails (the git-coder ends `blocked` or `failed`: no remote, push rejected,
`gh` missing or not logged in, a check fails, merge conflict), stop, change nothing more, and ask
the user with `chat_ask`, saying which steps did finish.

## Git routing and completion

  7. **Git routing and completion.** On calling `task_move done`, the hub routes the card to the
     first column whose auto-switch is OFF (To commit, To push, To open PR, To merge), or to Done
     if all are ON. It returns the ON steps passed along the way in `git_steps` (`commit`, `push`,
     `open_pr`, `merge_pr`) and the task's branch name in `branch`. Run the returned `git_steps`
     immediately, in that order, with the `git-coder`, and pass it that `branch` (ON steps
     announced in chat, as in *Git and pull requests*).
     - The hub also stops a card when the step cannot run: in To push when the repository has no
       git remote, in To open PR when `gh` is not installed. The card's log says why; tell the
       user what is missing.
     - A card in a git column waits for the user's OK to proceed with that step (via `chat_ask` or
       by the user dragging the card forward).
     - When the user drags the card forward along the line, the hub tells you which steps were
       approved; run them with the `git-coder`.
     - When moving a card out of a git column whose switch is OFF yourself: with the user's OK, call `task_move done reason="<quote>"`; run the returned git_steps (they include the step just OK'd).
     - **When a step fails** (the git-coder ends `blocked` or `failed`: push rejected, `gh` not
       logged in, checks red, merge conflict, the branch name already exists), the card may sit
       further along than the repository. You cannot move it back: tell the user with `chat_ask`
       what failed, where the card is and what is still not done. Never retry with force.
     - **A legacy To push card** (from before the pull-request flow) whose `jgt/<key>` branch no
       longer exists was already merged into the local default branch. The team does not push
       that branch: start no git-coder, and ask the user with `chat_ask` to push it themselves
       and move the card to Done.
     - The team can never move a card back from a git column to Working, Testing or Review.
     - Git columns do not count toward the WIP limit (the work is finished; only git steps await OK).
     - On reaching Done: `jg-team-dev run finish`, then tell the user the task is finished (and
       mention the follow-up task, if any).
