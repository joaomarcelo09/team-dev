# Toy notes app: delete a note

## Goal
Let a user remove a note that is no longer needed.

## Scope
Add `DELETE /notes/{id}` to the API and a delete button for each note on the notes page.

## Out of scope
Editing notes, authentication, pagination and changes to the data store.

## Acceptance criteria
- `curl -X DELETE http://localhost:8000/notes/1` removes note 1, and a later `GET /notes` no longer lists it.
- `pytest` in `e2e/toy/api` passes and covers deleting an existing note plus a `404` for an unknown id.
- `npm test` in `e2e/toy/web` passes and covers the delete button removing the note from the list.
- `npm run typecheck` and `npm run build` in `e2e/toy/web` pass.

## Constraints
Keep the existing layered API structure and the notes page layout; do not add new dependencies.
