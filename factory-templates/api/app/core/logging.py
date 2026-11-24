"""JSON structured logging."""

import json
import logging
from datetime import UTC, datetime
from typing import Any

EXTRA_FIELDS = ("method", "path", "status_code", "duration_ms", "code")


class JsonFormatter(logging.Formatter):
    """Render every log record as a single JSON object."""

    def format(self, record: logging.LogRecord) -> str:
        """Format the record as JSON, including selected structured fields."""
        payload: dict[str, Any] = {
            "timestamp": datetime.now(UTC).isoformat(),
            "level": record.levelname,
            "logger": record.name,
            "message": record.getMessage(),
        }
        for field in EXTRA_FIELDS:
            value = getattr(record, field, None)
            if value is not None:
                payload[field] = value
        if record.exc_info:
            payload["exception"] = self.formatException(record.exc_info)
        return json.dumps(payload, default=str)


def configure_logging(level: str) -> None:
    """Send all application logs to stdout in JSON format."""
    handler = logging.StreamHandler()
    handler.setFormatter(JsonFormatter())

    root = logging.getLogger()
    root.handlers.clear()
    root.addHandler(handler)
    root.setLevel(level.upper())
