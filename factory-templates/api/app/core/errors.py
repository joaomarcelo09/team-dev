"""Application errors and their HTTP handlers."""

import logging
from typing import Any

from fastapi import Request
from fastapi.encoders import jsonable_encoder
from fastapi.exceptions import RequestValidationError
from fastapi.responses import JSONResponse
from starlette.exceptions import HTTPException as StarletteHTTPException

from app.schemas.error import ErrorDetail, ErrorResponse

logger = logging.getLogger(__name__)

HTTP_ERROR_CODES = {404: "not_found", 405: "method_not_allowed"}


class AppError(Exception):
    """Base error that the API renders in the uniform error format."""

    def __init__(
        self, message: str, *, status_code: int = 500, code: str = "internal_error"
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status_code = status_code
        self.code = code


class NotFoundError(AppError):
    """Raised when a requested resource does not exist."""

    def __init__(self, message: str) -> None:
        super().__init__(message, status_code=404, code="not_found")


def error_response(
    *,
    status_code: int,
    code: str,
    message: str,
    details: list[Any] | None = None,
) -> JSONResponse:
    """Build a JSON response with the uniform error body."""
    body = ErrorResponse(error=ErrorDetail(code=code, message=message, details=details))
    return JSONResponse(status_code=status_code, content=body.model_dump(mode="json"))


def app_error_handler(request: Request, exc: Exception) -> JSONResponse:
    """Render an :class:`AppError` with its own status code and code."""
    error = exc if isinstance(exc, AppError) else AppError("Internal server error")
    logger.warning(
        "request_failed",
        extra={"path": request.url.path, "status_code": error.status_code, "code": error.code},
    )
    return error_response(
        status_code=error.status_code,
        code=error.code,
        message=error.message,
    )


def validation_exception_handler(_request: Request, exc: Exception) -> JSONResponse:
    """Render request validation failures in the uniform error format."""
    details = jsonable_encoder(exc.errors()) if isinstance(exc, RequestValidationError) else []
    return error_response(
        status_code=422,
        code="validation_error",
        message="Request validation failed",
        details=details,
    )


def http_exception_handler(_request: Request, exc: Exception) -> JSONResponse:
    """Render framework HTTP errors, such as unknown routes, in the uniform format."""
    if isinstance(exc, StarletteHTTPException):
        status_code = exc.status_code
        message = exc.detail if isinstance(exc.detail, str) else "Request failed"
    else:
        status_code = 500
        message = "Internal server error"
    return error_response(
        status_code=status_code,
        code=HTTP_ERROR_CODES.get(status_code, "http_error"),
        message=message,
    )


def unhandled_exception_handler(request: Request, _exc: Exception) -> JSONResponse:
    """Render unexpected failures as a generic 500 without leaking details."""
    logger.exception("unhandled_exception", extra={"path": request.url.path})
    return error_response(
        status_code=500,
        code="internal_error",
        message="Internal server error",
    )
