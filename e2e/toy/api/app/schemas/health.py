"""Schema for the health endpoint."""

from pydantic import BaseModel


class HealthResponse(BaseModel):
    """Liveness response."""

    status: str
