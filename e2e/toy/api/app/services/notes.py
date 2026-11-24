"""Note service: note business logic."""

from app.db.models import Note
from app.repositories.notes import NoteRepository
from app.schemas.note import NoteCreate


class NoteService:
    """Create and list notes through the note repository."""

    def __init__(self, repository: NoteRepository) -> None:
        self._repository = repository

    async def create_note(self, payload: NoteCreate) -> Note:
        """Create a note from its API payload."""
        note = Note(text=payload.text)
        return await self._repository.add(note)

    async def list_notes(self) -> list[Note]:
        """Return every note."""
        return await self._repository.list_all()
