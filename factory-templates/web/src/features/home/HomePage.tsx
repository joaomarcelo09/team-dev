import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { applyTheme, getInitialTheme, type Theme } from "@/lib/theme";

/** Example feature page that exercises the design tokens, a button and a theme switch. */
export function HomePage() {
  const [theme, setTheme] = useState<Theme>(getInitialTheme);
  const [count, setCount] = useState(0);

  useEffect(() => {
    applyTheme(theme);
  }, [theme]);

  function toggleTheme() {
    setTheme((current) => (current === "dark" ? "light" : "dark"));
  }

  return (
    <main className="mx-auto flex min-h-screen max-w-3xl flex-col justify-center gap-6 p-8">
      <div className="space-y-2">
        <p className="text-sm font-medium text-muted-foreground">Example feature</p>
        <h1 className="text-3xl font-bold tracking-tight">Home</h1>
        <p className="text-muted-foreground">
          React, TypeScript, Vite, Tailwind, React Router and TanStack Query in a small starting
          point.
        </p>
      </div>
      <div className="flex flex-wrap items-center gap-3">
        <Button onClick={() => setCount((value) => value + 1)}>Count is {count}</Button>
        <Button variant="outline" onClick={toggleTheme}>
          Switch to {theme === "dark" ? "light" : "dark"} mode
        </Button>
      </div>
    </main>
  );
}
