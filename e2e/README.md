# E2E fixtures

`toy/` is a self-contained toy project used by the Dark Factory E2E runner: a small FastAPI notes
API (`toy/api`, `GET/POST /notes` over SQLite) and a React notes page (`toy/web`) that lists those
notes. The runner copies the fixture into a scratch directory and runs a real coder round against
`toy/SPEC.md`, which asks for one small feature (`DELETE /notes/{id}` plus a delete button), then
checks the API and web tests and keeps the fixture itself unchanged.

The fixture has one CI workflow at `toy/.github/workflows/ci.yml`, at the repository root where
the runner pushes `toy/`. Its API and web jobs run their checks from `api/` and `web/`
respectively. Keep workflows at this root; GitHub does not discover workflows nested under those
project directories.
