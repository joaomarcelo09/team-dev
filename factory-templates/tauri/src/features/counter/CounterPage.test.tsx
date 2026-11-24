import { invoke } from "@tauri-apps/api/core";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

import { CounterPage } from "@/features/counter/CounterPage";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

const invokeMock = vi.mocked(invoke);

describe("CounterPage", () => {
  beforeEach(() => {
    invokeMock.mockReset();
  });

  it("renders the example page", () => {
    render(<CounterPage />);

    expect(screen.getByRole("heading", { name: "Counter" })).toBeInTheDocument();
  });

  it("shows the value returned by the increment command", async () => {
    const user = userEvent.setup();
    invokeMock.mockResolvedValue(3);

    render(<CounterPage />);
    await user.click(screen.getByRole("button", { name: /increment counter/i }));

    expect(invokeMock).toHaveBeenCalledWith("increment_counter");
    expect(await screen.findByText("Count: 3")).toBeInTheDocument();
  });
});
