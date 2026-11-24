# {{project_name}}

A FastAPI backend starting point: layered `routers -> services -> repositories`, Pydantic v2
schemas, SQLAlchemy 2.0 async with Alembic migrations, PostgreSQL, environment-based settings,
JSON structured logging and one uniform error format. `items` is the single example resource and
has its tests; `GET /health` is the liveness endpoint.

Replace the project name placeholder in this title and in the `name` field of `pyproject.toml`
when you start a project from this template.

## Requirements

- Python 3.12 or newer
- PostgreSQL 16 (runtime database)

## Run

```sh
python -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"
cp .env.example .env
alembic upgrade head
uvicorn app.main:app --reload
```

The API serves at http://localhost:8000, with the interactive docs at
http://localhost:8000/docs.

Settings are read from environment variables (or the local `.env`): `APP_NAME`, `ENVIRONMENT`,
`LOG_LEVEL` and `DATABASE_URL`. `.env.example` lists them with development placeholders.

## Test

```sh
pytest               # unit tests, no PostgreSQL needed
ruff check .         # lint
ruff format --check  # formatting
mypy .               # strict type check
```

The tests override the database session dependency with an in-memory SQLite database
(`aiosqlite`), so they run without PostgreSQL. PostgreSQL is the runtime and migration database.

The migration path is checked against PostgreSQL with:

```sh
alembic upgrade head
```

## API

| Method | Path           | Description                       |
| ------ | -------------- | --------------------------------- |
| GET    | `/health`      | Liveness check                    |
| POST   | `/items`       | Create an item                    |
| GET    | `/items/{id}`  | Read an item; 404 when missing    |

Every error uses the same body:

```json
{
  "error": {
    "code": "not_found",
    "message": "Item 42 was not found",
    "details": null
  }
}
```

## Structure

```text
app/
  core/           settings, logging, errors and request middleware
  db/             declarative base, models and the async session dependency
  repositories/   database access for one resource
  routers/        HTTP endpoints (health and items)
  schemas/        Pydantic request and response models
  services/       business logic
  dependencies.py dependency wiring for routers
  main.py         application factory and ASGI entry point
alembic/          async migration environment and versions
tests/            httpx tests against the app with a SQLite override
```

## Migrations

Create a new migration after changing a model:

```sh
alembic revision --autogenerate -m "describe the change"
```

Review the generated file in `alembic/versions/` before applying it.

## Standards

This template follows the lab standards pack version 1.0.0. The tracked version is in
`factory-templates/STANDARDS_VERSION`; the rules are in `factory-skills/standards/STANDARDS.md`.
