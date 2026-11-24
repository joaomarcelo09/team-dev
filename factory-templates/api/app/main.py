"""Application factory and ASGI entry point."""

from fastapi import FastAPI
from fastapi.exceptions import RequestValidationError
from starlette.exceptions import HTTPException as StarletteHTTPException

from app.core.errors import (
    AppError,
    app_error_handler,
    http_exception_handler,
    unhandled_exception_handler,
    validation_exception_handler,
)
from app.core.logging import configure_logging
from app.core.middleware import RequestLoggingMiddleware
from app.core.settings import get_settings
from app.routers import health, items


def create_app() -> FastAPI:
    """Build the FastAPI application with routes, logging and error handling."""
    settings = get_settings()
    configure_logging(settings.log_level)

    app = FastAPI(title=settings.app_name)
    app.add_middleware(RequestLoggingMiddleware)
    app.add_exception_handler(AppError, app_error_handler)
    app.add_exception_handler(StarletteHTTPException, http_exception_handler)
    app.add_exception_handler(RequestValidationError, validation_exception_handler)
    app.add_exception_handler(Exception, unhandled_exception_handler)
    app.include_router(health.router)
    app.include_router(items.router)
    return app


app = create_app()
