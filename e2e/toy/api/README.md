# Toy notes API

The API half of the Dark Factory E2E toy project. It is a trimmed copy of `factory-templates/api`:
a FastAPI app with layered `routers -> services -> repositories`, Pydantic schemas, async
SQLAlchemy (SQLite), structured logging and one uniform error format. `notes` is the only resource.

## Requirements

- Python 3.12 or newer

## Run

```sh
python -m venv .venv
source .venv/bin/activate
pip install -e ".[dev]"
uvicorn app.main:app --reload
```

The API serves at http://localhost:8000. The SQLite database file is created on first use.

## Test

```sh
pytest               # unit tests, no external database needed
ruff check .         # lint
ruff format --check  # formatting
mypy .               # strict type check
```

## API

| Method | Path     | Description           |
| ------ | -------- | --------------------- |
| GET    | `/health`| Liveness check        |
| GET    | `/notes` | List notes            |
| POST   | `/notes` | Create a note         |

Every error uses the same body:

```json
{
  "error": {
    "code": "validation_error",
    "message": "Request validation failed",
    "details": []
  }
}
```
