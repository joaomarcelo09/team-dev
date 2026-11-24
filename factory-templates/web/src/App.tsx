import { Route, Routes } from "react-router-dom";

import { HomePage } from "@/features/home/HomePage";

/** Application routes. */
export function App() {
  return (
    <Routes>
      <Route path="/" element={<HomePage />} />
    </Routes>
  );
}
