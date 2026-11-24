export type Theme = "light" | "dark";

/** Returns the theme that matches the user's system preference, defaulting to light. */
export function getInitialTheme(): Theme {
  const prefersDark =
    typeof window.matchMedia === "function" &&
    window.matchMedia("(prefers-color-scheme: dark)").matches;

  return prefersDark ? "dark" : "light";
}

/** Applies the theme to the document root so Tailwind's dark variant is active. */
export function applyTheme(theme: Theme): void {
  document.documentElement.classList.toggle("dark", theme === "dark");
}
