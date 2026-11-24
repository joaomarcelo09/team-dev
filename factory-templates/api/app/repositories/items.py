"""Item repository: all item database access lives here."""

from sqlalchemy.ext.asyncio import AsyncSession

from app.db.models import Item


class ItemRepository:
    """Read and write items through the given session."""

    def __init__(self, session: AsyncSession) -> None:
        self._session = session

    async def add(self, item: Item) -> Item:
        """Persist a new item and return it with its generated fields."""
        self._session.add(item)
        await self._session.commit()
        await self._session.refresh(item)
        return item

    async def get(self, item_id: int) -> Item | None:
        """Return the item with the given id, or ``None`` when it does not exist."""
        return await self._session.get(Item, item_id)
