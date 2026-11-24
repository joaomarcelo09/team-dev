"""Note repository: all note database access lives here."""

from sqlalchemy import select
from sqlalchemy.ext.asyncio import AsyncSession

from app.db.models import Note


class NoteRepository:
    """Read and write notes through the given session."""

    def __init__(self, session: AsyncSession) -> None:
        self._session = session

    async def add(self, note: Note) -> Note:
        """Persist a new note and return it with its generated fields."""
        self._session.add(note)
        await self._session.commit()
        await self._session.refresh(note)
        return note

    async def list_all(self) -> list[Note]:
        """Return every note, oldest first."""
        result = await self._session.scalars(select(Note).order_by(Note.id))
        return list(result)
