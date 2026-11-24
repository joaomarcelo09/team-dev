"""Note endpoints."""

from typing import Annotated

from fastapi import APIRouter, Depends, status

from app.dependencies import get_note_service
from app.schemas.error import ErrorResponse
from app.schemas.note import NoteCreate, NoteRead
from app.services.notes import NoteService

router = APIRouter(prefix="/notes", tags=["notes"])

NoteServiceDep = Annotated[NoteService, Depends(get_note_service)]


@router.get("", response_model=list[NoteRead])
async def list_notes(service: NoteServiceDep) -> list[NoteRead]:
    """Return every note, oldest first."""
    return [NoteRead.model_validate(note) for note in await service.list_notes()]


@router.post(
    "",
    response_model=NoteRead,
    status_code=status.HTTP_201_CREATED,
    responses={422: {"model": ErrorResponse}},
)
async def create_note(payload: NoteCreate, service: NoteServiceDep) -> NoteRead:
    """Create a note."""
    note = await service.create_note(payload)
    return NoteRead.model_validate(note)
