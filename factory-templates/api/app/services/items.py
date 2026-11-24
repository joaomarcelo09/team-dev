"""Item service: item business logic."""

from app.core.errors import NotFoundError
from app.db.models import Item
from app.repositories.items import ItemRepository
from app.schemas.item import ItemCreate


class ItemService:
    """Create and fetch items through the item repository."""

    def __init__(self, repository: ItemRepository) -> None:
        self._repository = repository

    async def create_item(self, payload: ItemCreate) -> Item:
        """Create an item from its API payload."""
        item = Item(name=payload.name, description=payload.description)
        return await self._repository.add(item)

    async def get_item(self, item_id: int) -> Item:
        """Return an item or raise :class:`NotFoundError`."""
        item = await self._repository.get(item_id)
        if item is None:
            raise NotFoundError(f"Item {item_id} was not found")
        return item
