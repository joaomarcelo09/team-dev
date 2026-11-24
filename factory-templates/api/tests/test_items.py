"""Item resource tests."""

from httpx import AsyncClient


async def test_create_item(client: AsyncClient) -> None:
    """Creating an item returns it with an id."""
    response = await client.post("/items", json={"name": "Widget", "description": "A widget"})

    assert response.status_code == 201
    body = response.json()
    assert body["id"] == 1
    assert body["name"] == "Widget"
    assert body["description"] == "A widget"


async def test_get_item(client: AsyncClient) -> None:
    """An item can be read back by id."""
    created = await client.post("/items", json={"name": "Widget"})
    item_id = created.json()["id"]

    response = await client.get(f"/items/{item_id}")

    assert response.status_code == 200
    assert response.json()["name"] == "Widget"


async def test_missing_item_uses_uniform_error(client: AsyncClient) -> None:
    """A missing item returns the uniform 404 error body."""
    response = await client.get("/items/999")

    assert response.status_code == 404
    assert response.json() == {
        "error": {
            "code": "not_found",
            "message": "Item 999 was not found",
            "details": None,
        }
    }


async def test_invalid_payload_uses_uniform_error(client: AsyncClient) -> None:
    """A validation failure returns the uniform 422 error body."""
    response = await client.post("/items", json={"name": ""})

    assert response.status_code == 422
    body = response.json()
    assert body["error"]["code"] == "validation_error"
    assert body["error"]["message"] == "Request validation failed"
    assert body["error"]["details"]
