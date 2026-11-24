# Project templates

Starting points for new lab projects, one per stack:

| Template | Stack                                                                 |
| -------- | --------------------------------------------------------------------- |
| `api/`   | FastAPI, Pydantic v2, SQLAlchemy 2.0 async, Alembic and PostgreSQL    |
| `web/`   | React 18, TypeScript strict, Vite, Tailwind CSS, shadcn/ui and Vitest |
| `tauri/` | Tauri 2 desktop app: React frontend and a Rust `app-core` crate       |

Each template is a minimal runnable skeleton with one example feature and its unit test, a
lint/format configuration, a project `README.md` (what it is, how to run, how to test) and a
GitHub Actions workflow in `.github/workflows/ci.yml` (unit tests, lint/format, type check and
build, with pinned action majors and `permissions: contents: read`).

## Start a new project

1. Copy the template directory to the new project, without its `.git` or CI run history.
2. Replace `{{project_name}}` in the `README.md` title and in the package name (`pyproject.toml`
   for `api`, `package.json` for `web` and `tauri`).
3. Follow the template `README.md` to install dependencies, run and test the skeleton.
4. Keep the example feature as the pattern for the first real feature; delete it only when the
   spec replaces it.
5. Commit the lockfiles the first install/build generates.

## Standards pack

The templates implement the lab standards pack in `factory-skills/standards/STANDARDS.md`, which
is versioned with SemVer in `factory-skills/standards/VERSION`. `STANDARDS_VERSION` in this
directory records the pack version the templates follow; `check.sh` fails when the two differ.
A project may override a rule only when its spec says so explicitly.

## Check

```sh
./check.sh
```

The script runs offline and fails when a template lacks its `README.md` or CI workflow, when a
workflow lacks lint, format, test, type-check or build steps, when `STANDARDS_VERSION` differs
from the pack `VERSION`, or when a YAML, JSON or TOML file does not parse. It needs only POSIX
`sh`, `grep` and `python3`.
