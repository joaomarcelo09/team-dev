import { Route, Routes } from "react-router-dom";

import { NotesPage } from "@/features/notes/NotesPage";

/** Application routes. */
export function App() {
  return (
    <Routes>
      <Route path="/" element={<NotesPage />} />
    </Routes>
  );
}
