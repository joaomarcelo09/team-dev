"""Item endpoints."""

from typing import Annotated

from fastapi import APIRouter, Depends, status

from app.dependencies import get_item_service
from app.schemas.error import ErrorResponse
from app.schemas.item import ItemCreate, ItemRead
from app.services.items import ItemService

router = APIRouter(prefix="/items", tags=["items"])

ItemServiceDep = Annotated[ItemService, Depends(get_item_service)]


@router.post(
    "",
    response_model=ItemRead,
    status_code=status.HTTP_201_CREATED,
    responses={422: {"model": ErrorResponse}},
)
async def create_item(payload: ItemCreate, service: ItemServiceDep) -> ItemRead:
    """Create an item."""
    item = await service.create_item(payload)
    return ItemRead.model_validate(item)


@router.get(
    "/{item_id}",
    response_model=ItemRead,
    responses={404: {"model": ErrorResponse}},
)
async def get_item(item_id: int, service: ItemServiceDep) -> ItemRead:
    """Return one item by id."""
    item = await service.get_item(item_id)
    return ItemRead.model_validate(item)
