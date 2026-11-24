# {{project_name}}

A web app starting point: React 18, TypeScript in strict mode, Vite, Tailwind CSS with
CSS-variable design tokens (light and dark), shadcn/ui, React Router and TanStack Query.
`src/features/home` is the single example feature and has its Vitest test.

Replace the project name placeholder in this title and in the `name` field of `package.json` when
you start a project from this template.

## Requirements

- Node.js 20 or newer
- npm

## Run

```sh
npm install
npm run dev
```

Vite serves the app at http://localhost:5173.

## Test

```sh
npm test             # Vitest + Testing Library, one run
npm run lint         # ESLint
npm run format:check # Prettier
npm run typecheck    # tsc --noEmit
npm run build        # production build in dist/
```

## Structure

```text
src/
  components/ui/     shadcn/ui components (button is the example)
  features/home/     example feature and its test
  lib/               shared helpers (class names, query client, theme)
  test/              Vitest setup
  App.tsx            routes
  index.css          design tokens and Tailwind layers
  main.tsx           providers: TanStack Query client and React Router
```

Design tokens are CSS variables in `src/index.css` (the `:root` and `.dark` blocks) and are mapped
to Tailwind in `tailwind.config.ts`. The home page toggles the `dark` class on `<html>`.

## Package lockfile

The template ships without `package-lock.json` because the file cannot be generated offline. The
first `npm install` in a new project creates it; commit it and use `npm ci` in CI from then on.
The included workflow uses `npm install` for the same reason.

## Standards

This template follows the lab standards pack version 1.0.0. The tracked version is in
`factory-templates/STANDARDS_VERSION`; the rules are in `factory-skills/standards/STANDARDS.md`.
