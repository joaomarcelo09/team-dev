# Writing a brief

- **The brief is everything the worker knows, and it is short.** Write it to a file under
  `.team/briefs/` with the goal, the scope, what must not change, the acceptance criteria and,
  for a reviewer, the exact checks. Every worker pays to read it:
  - Copy **only the plan section for this task**, with its acceptance criteria. Never list whole
    documents to read (a PRD, the full plan, a decisions file).
  - Name the **files to open**, not documents to study: each with the symbol to change and its
    approximate lines (`crates/x/src/y.rs`, `fn inline_config`, ~130), so the coder greps and reads
    a window instead of the whole file.
  - Give the exact commands for history or restores (for example `git show <sha>`), so the coder
    never needs the web to read this repository.
  - Say what must not be touched, and ask for readable code in the style of the neighbouring code.
  - Add the line "Do not re-read: <the PRD, the full plan>; they are summarized above."
  - Aim for under 60 lines. A brief for fixing findings, or for a reviewer after one, aims for under 20.
  - End every brief with the line "Report once when done, short: no progress messages."
