"""Dependency wiring used by the routers."""

from typing import Annotated

from fastapi import Depends
from sqlalchemy.ext.asyncio import AsyncSession

from app.db.session import get_session
from app.repositories.items import ItemRepository
from app.services.items import ItemService

SessionDep = Annotated[AsyncSession, Depends(get_session)]


def get_item_service(session: SessionDep) -> ItemService:
    """Build the item service with its repository for one request."""
    return ItemService(ItemRepository(session))
