import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { NotesPage } from "@/features/notes/NotesPage";

function renderNotesPage() {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={queryClient}>
      <NotesPage />
    </QueryClientProvider>,
  );
}

function mockNotesResponse(notes: unknown[] | null, ok = true) {
  const fetchMock = vi.fn().mockResolvedValue({
    ok,
    status: ok ? 200 : 500,
    json: async () => notes,
  });
  vi.stubGlobal("fetch", fetchMock);
  return fetchMock;
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("NotesPage", () => {
  it("lists the notes returned by the API", async () => {
    mockNotesResponse([
      { id: 1, text: "Buy milk", created_at: "2026-01-01T00:00:00Z" },
      { id: 2, text: "Walk the dog", created_at: "2026-01-02T00:00:00Z" },
    ]);
    renderNotesPage();

    expect(await screen.findByText("Buy milk")).toBeInTheDocument();
    expect(screen.getByText("Walk the dog")).toBeInTheDocument();
  });

  it("shows an empty state when there are no notes", async () => {
    mockNotesResponse([]);
    renderNotesPage();

    expect(await screen.findByText("No notes yet.")).toBeInTheDocument();
  });

  it("shows an error when the API request fails", async () => {
    mockNotesResponse(null, false);
    renderNotesPage();

    expect(await screen.findByRole("alert")).toHaveTextContent("Could not load notes.");
  });
});
