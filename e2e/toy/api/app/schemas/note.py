"""Note schemas."""

from datetime import datetime

from pydantic import BaseModel, ConfigDict, Field


class NoteCreate(BaseModel):
    """Payload for creating a note."""

    text: str = Field(min_length=1, max_length=2000)


class NoteRead(BaseModel):
    """Note representation returned by the API."""

    model_config = ConfigDict(from_attributes=True)

    id: int
    text: str
    created_at: datetime
