# {{project_name}}

A Tauri 2 desktop app starting point: a React 18 + TypeScript (strict) frontend built with Vite,
and a Rust Cargo workspace with `src-tauri` for the window and thin commands plus the `app-core`
crate for the logic. SQLite storage uses rusqlite. The single example feature persists a counter and has
frontend and Rust unit tests.

Replace the project name placeholder in this title and in the `name` field of `package.json` when
you start a project from this template.

## Requirements

- Node.js 20 or newer and npm
- Rust stable (1.90 or newer) with rustfmt and clippy (`rustup component add rustfmt clippy`)
- On Linux, Tauri's system libraries, for example on Debian/Ubuntu:

```sh
sudo apt-get install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
  librsvg2-dev libxdo-dev libssl-dev
```

## Run

```sh
npm install
npm run tauri dev
```

The frontend dev server runs at http://localhost:1420 and Tauri opens the desktop window. On first
launch the app creates `counter.sqlite3` in the platform app data directory.

## Test

Frontend, from the template root:

```sh
npm run lint
npm run format:check
npm run typecheck
npm test
npm run build
```

Rust, from `src-tauri` (run `npm run build` first, because the app embeds the frontend `dist`
at compile time):

```sh
cd src-tauri
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p app-core
cargo build
```

## Structure

```text
src/
  features/counter/  example feature, its Tauri command call and its Vitest test
  test/              Vitest setup
  App.tsx            application root
  index.css          minimal design tokens for light and dark
  main.tsx           React entry point
src-tauri/
  core/              domain logic and SQLite storage, with unit tests
  src/               window setup, shared state and thin commands
  capabilities/      Tauri 2 capability for the main window
  tauri.conf.json    window, build and bundle configuration
```

The frontend calls the `increment_counter` command in `src-tauri/src/commands.rs`; the command
delegates to `CounterStore::increment` in the `app-core` crate, which owns the SQLite schema and
the unit tests.

## Icons

The icons under `src-tauri/icons` are placeholder PNG, ICO and ICNS files. Generate branded icons
with `npm run tauri icon path/to/icon.png`.

## Lockfiles

The template ships without `package-lock.json` or `Cargo.lock` because they cannot be generated
offline. The first `npm install` and `cargo build` create them; commit both and use `npm ci` in CI
from then on.

## Standards

This template follows the lab standards pack version 1.0.0. The tracked version is in
`factory-templates/STANDARDS_VERSION`; the rules are in `factory-skills/standards/STANDARDS.md`.
