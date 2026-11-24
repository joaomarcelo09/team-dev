"""Pydantic schemas for the uniform error response."""

from typing import Any

from pydantic import BaseModel


class ErrorDetail(BaseModel):
    """Machine-readable error code, human message and optional validation details."""

    code: str
    message: str
    details: list[Any] | None = None


class ErrorResponse(BaseModel):
    """Body returned for every error response."""

    error: ErrorDetail
