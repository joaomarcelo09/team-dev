"""Health endpoint tests."""

from httpx import AsyncClient


async def test_health(client: AsyncClient) -> None:
    """The health endpoint reports that the API is running."""
    response = await client.get("/health")

    assert response.status_code == 200
    assert response.json() == {"status": "ok"}
