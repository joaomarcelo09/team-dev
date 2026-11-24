"""Dependency wiring used by the routers."""

from typing import Annotated

from fastapi import Depends
from sqlalchemy.ext.asyncio import AsyncSession

from app.db.session import get_session
from app.repositories.notes import NoteRepository
from app.services.notes import NoteService

SessionDep = Annotated[AsyncSession, Depends(get_session)]


def get_note_service(session: SessionDep) -> NoteService:
    """Build the note service with its repository for one request."""
    return NoteService(NoteRepository(session))
