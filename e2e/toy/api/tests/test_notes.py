"""Note resource tests."""

from httpx import AsyncClient


async def test_list_notes_is_empty_at_first(client: AsyncClient) -> None:
    """A fresh database has no notes."""
    response = await client.get("/notes")

    assert response.status_code == 200
    assert response.json() == []


async def test_create_note(client: AsyncClient) -> None:
    """Creating a note returns it with an id."""
    response = await client.post("/notes", json={"text": "Buy milk"})

    assert response.status_code == 201
    body = response.json()
    assert body["id"] == 1
    assert body["text"] == "Buy milk"


async def test_list_notes_returns_created_notes(client: AsyncClient) -> None:
    """Notes are listed oldest first."""
    await client.post("/notes", json={"text": "First"})
    await client.post("/notes", json={"text": "Second"})

    response = await client.get("/notes")

    assert response.status_code == 200
    assert [note["text"] for note in response.json()] == ["First", "Second"]


async def test_invalid_payload_uses_uniform_error(client: AsyncClient) -> None:
    """A validation failure returns the uniform 422 error body."""
    response = await client.post("/notes", json={"text": ""})

    assert response.status_code == 422
    body = response.json()
    assert body["error"]["code"] == "validation_error"
    assert body["error"]["message"] == "Request validation failed"
    assert body["error"]["details"]
