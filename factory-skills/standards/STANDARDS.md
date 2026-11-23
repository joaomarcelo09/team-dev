# Lab standards

The lab's single standards pack, versioned in the lab. Every project brief, task and review
refers to it, and the reviewer checks it as a documented-pattern gate. Its purpose: coders never
re-decide stack, UI or architecture per project.

**A project may override a rule only if its spec says so explicitly.** The override names the
rule, the replacement and the reason. A silent difference from this pack is a review finding.

## How to use this pack

- Read the sections for the project's stack; where the spec is silent, this pack is the default.
- Apply the checklists as written; do not invent extra architecture around them.
- The tester derives checks from the spec, not from this pack. The reviewer checks this pack.

## Design system

- [ ] Shared design tokens for color, spacing, type, radius and dark/light, as a Tailwind theme
      plus CSS variables.
- [ ] One component library on shadcn/ui (Radix primitives) in a shared `ui` package or folder;
      no per-project bespoke primitives when a shared one exists.
- [ ] Accessibility baseline: semantic HTML, labels, keyboard access, visible focus, contrast.
- [ ] Shared layout, form, table and empty/loading/error state patterns.
- [ ] No hardcoded color, spacing or type value where a token exists.

## Frontend

- [ ] React + TypeScript in strict mode, built with Vite.
- [ ] React Router for routing.
- [ ] TanStack Query for server state; Zustand for client state. Do not mix the two jobs.
- [ ] react-hook-form with zod for forms and validation.
- [ ] Feature-folder structure, following the project's existing folders.
- [ ] Vitest with Testing Library for unit/component tests; Playwright for e2e.
- [ ] Lint and format clean.

## Backend (FastAPI)

- [ ] Layered `routers -> services -> repositories`; no database access in routers.
- [ ] Pydantic v2 schemas at the API boundary and between layers where useful.
- [ ] SQLAlchemy 2.0 async with Alembic migrations; PostgreSQL.
- [ ] Settings from the environment, never literals; structured logging; one uniform error format.
- [ ] An auth module pattern, only when the spec asks for auth (see *Auth* below).
- [ ] Async or heavy jobs through a Redis queue (arq or Celery, one lab-wide choice) only for
      spec'd work.
- [ ] pytest with httpx for API tests; migration tests where the project has migrations.
- [ ] Lint and format clean (ruff).

## Tauri apps

- [ ] React frontend per the *Frontend* checklist.
- [ ] Rust commands are thin; logic lives in a core crate with its own tests.
- [ ] Local storage on SQLite (sqlx or rusqlite, one lab-wide choice).
- [ ] `cargo test` for the core crate.
- [ ] v1 acceptance is build plus unit/integration tests; headless GUI runs are out of scope.
- [ ] clippy and rustfmt clean.

## Scope: no speculative work

- [ ] Build only what the spec and its acceptance criteria require.
- [ ] Default architecture is the simplest that works: a modular monolith, one API.
- [ ] No microservices, queues, caches, auth, extra services or abstractions unless the spec
      requires them. Redis, for example, only for spec'd async or heavy work.
- [ ] No unused dependencies, endpoints, flags or configuration.
- [ ] The reviewer flags anything beyond the spec as a finding.

## Clean code

- [ ] Shared and well-used functions live in helper/utility modules; no copy-paste.
- [ ] Imports at the top of the file only, never inside a function.
- [ ] Small, single-purpose functions and files; clear names.
- [ ] Docstrings/doc comments on public modules, functions and API endpoints.
- [ ] A short project README: what it is, how to run it, how to test it.
- [ ] Follow the lab patterns and the project's existing patterns before inventing new ones.
- [ ] No dead code, no commented-out code, no unused dependencies.
- [ ] Lint and format clean.

## Commits

- [ ] Conventional Commits in English: `type(scope): summary`, imperative, one concern per commit.
- [ ] No attribution of any kind, on every harness: no `Co-Authored-By`, no "Generated with",
      no tool signature, in commits or PR bodies.
- [ ] Workers do not commit. The git-coder commits, pushes and opens PRs through the host-side
      broker, which enforces the message rule and the allowed action list.

## Auth

- [ ] Never implement auth unless the spec asks for it: no speculative login, users, roles or
      tokens.
- [ ] *What* auth a project needs is decided per project in the spec. When the spec asks for it,
      *how* is this default:
  - [ ] JWT with a short-lived access token and a refresh token.
  - [ ] Password hashing with argon2 or bcrypt.
  - [ ] A FastAPI dependency for the current user; role/permission checks in the service layer.
  - [ ] Tokens never logged; secrets from the environment.
- [ ] A spec overrides the default only by saying so explicitly.

## Shared tooling

- [ ] ESLint + Prettier (web), ruff (Python), clippy + rustfmt (Rust).
- [ ] Conventional commits, as above.
- [ ] A per-project "how to run" recipe the lab can execute.
- [ ] Project skeleton templates (web, api, tauri) so a new project starts from them.
- [ ] CI on every project: at minimum unit tests, plus lint/format, type check and build; the
      acceptance tests run in CI too.
- [ ] Pinned, minimal dependencies; lockfile policy as the spec states.

## Secrets

- [ ] Never read `.env` or `.env.*` (except `.env.example`, `.env.sample`, `.env.template`),
      private keys, credential files or cloud CLI config.
- [ ] Never print the environment, and never put a secret in code, logs, briefs, cards, chat,
      results or test fixtures.
- [ ] Need a secret? Stop and escalate; use `.env.example` and placeholders.
- [ ] Workers receive only the environment keys their project profile allows; outward and
      irreversible actions go through the host-side broker.
