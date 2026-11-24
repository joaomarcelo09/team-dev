"""Schemas for the example item resource."""

from datetime import datetime

from pydantic import BaseModel, ConfigDict, Field


class ItemCreate(BaseModel):
    """Payload for creating an item."""

    name: str = Field(min_length=1, max_length=255)
    description: str | None = Field(default=None, max_length=2000)


class ItemRead(BaseModel):
    """Item representation returned by the API."""

    model_config = ConfigDict(from_attributes=True)

    id: int
    name: str
    description: str | None
    created_at: datetime
