# Toy notes web

The web half of the Dark Factory E2E toy project. It is a trimmed copy of `factory-templates/web`:
React 18, TypeScript in strict mode, Vite, Tailwind CSS, shadcn/ui, React Router and TanStack
Query. `src/features/notes` is the only feature and lists the notes from the toy API through the
Vite `/notes` proxy.

## Requirements

- Node.js 20 or newer
- npm

## Run

Start the toy API on http://localhost:8000 first, then:

```sh
npm install
npm run dev
```

Vite serves the app at http://localhost:5173 and proxies `/notes` to the API.

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
  features/notes/    the notes page and its test
  lib/               shared helpers (class names, query client)
  test/              Vitest setup
  App.tsx            routes
  index.css          design tokens and Tailwind layers
  main.tsx           providers: TanStack Query client and React Router
```
