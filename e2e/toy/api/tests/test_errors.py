"""Uniform error format tests."""

from httpx import AsyncClient


async def test_unknown_route_uses_uniform_error(client: AsyncClient) -> None:
    """Framework 404s use the same error body as application errors."""
    response = await client.get("/missing")

    assert response.status_code == 404
    assert response.json() == {
        "error": {
            "code": "not_found",
            "message": "Not Found",
            "details": None,
        }
    }
