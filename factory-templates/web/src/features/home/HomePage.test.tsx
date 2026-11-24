import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MemoryRouter } from "react-router-dom";
import { describe, expect, it } from "vitest";

import { HomePage } from "@/features/home/HomePage";

function renderHomePage() {
  return render(
    <MemoryRouter>
      <HomePage />
    </MemoryRouter>,
  );
}

describe("HomePage", () => {
  it("renders the example page", () => {
    renderHomePage();

    expect(screen.getByRole("heading", { name: "Home" })).toBeInTheDocument();
  });

  it("increments the counter when the button is clicked", async () => {
    const user = userEvent.setup();
    renderHomePage();

    await user.click(screen.getByRole("button", { name: /count is 0/i }));

    expect(screen.getByRole("button", { name: /count is 1/i })).toBeInTheDocument();
  });

  it("toggles between light and dark mode", async () => {
    const user = userEvent.setup();
    renderHomePage();

    await user.click(screen.getByRole("button", { name: /switch to dark mode/i }));

    expect(document.documentElement).toHaveClass("dark");
    expect(screen.getByRole("button", { name: /switch to light mode/i })).toBeInTheDocument();
  });
});
