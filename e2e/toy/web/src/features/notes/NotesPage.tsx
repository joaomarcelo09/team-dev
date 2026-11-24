import { useQuery, type UseQueryResult } from "@tanstack/react-query";

interface Note {
  id: number;
  text: string;
  created_at: string;
}

async function fetchNotes(): Promise<Note[]> {
  const response = await fetch("/notes");
  if (!response.ok) {
    throw new Error(`Failed to load notes: ${response.status}`);
  }
  return response.json();
}

function NotesList({ notes }: { notes: UseQueryResult<Note[]> }) {
  if (notes.isPending) {
    return <p className="text-muted-foreground">Loading notes…</p>;
  }
  if (notes.isError) {
    return (
      <p role="alert" className="text-destructive">
        Could not load notes.
      </p>
    );
  }
  if (notes.data.length === 0) {
    return <p className="text-muted-foreground">No notes yet.</p>;
  }
  return (
    <ul className="divide-y rounded-md border">
      {notes.data.map((note) => (
        <li key={note.id} className="px-4 py-3">
          {note.text}
        </li>
      ))}
    </ul>
  );
}

/** Lists the notes returned by the toy API. */
export function NotesPage() {
  const notes = useQuery({ queryKey: ["notes"], queryFn: fetchNotes });

  return (
    <main className="mx-auto flex min-h-screen max-w-3xl flex-col gap-6 p-8">
      <h1 className="text-3xl font-bold tracking-tight">Notes</h1>
      <NotesList notes={notes} />
    </main>
  );
}
