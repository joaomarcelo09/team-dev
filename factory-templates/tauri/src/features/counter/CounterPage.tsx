import { invoke } from "@tauri-apps/api/core";
import { useState } from "react";

/** Example feature that persists a counter through the Rust core crate. */
export function CounterPage() {
  const [count, setCount] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  async function increment() {
    setPending(true);
    setError(null);

    try {
      const value = await invoke<number>("increment_counter");
      setCount(value);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setPending(false);
    }
  }

  return (
    <main className="app">
      <p className="eyebrow">Example feature</p>
      <h1>Counter</h1>
      <p>The Rust core crate stores this value in SQLite through one thin Tauri command.</p>
      <button type="button" disabled={pending} onClick={() => void increment()}>
        Increment counter
      </button>
      {count !== null && <p className="count">Count: {count}</p>}
      {error !== null && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
    </main>
  );
}
