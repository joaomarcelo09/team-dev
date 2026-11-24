//! Tauri commands: thin adapters that delegate to the `app-core` crate.

use tauri::State;

use crate::AppState;

/// Increments the persisted counter and returns its new value.
#[tauri::command]
pub fn increment_counter(state: State<'_, AppState>) -> Result<i64, String> {
    let counter =
        state.counter.lock().map_err(|error| format!("counter lock poisoned: {error}"))?;

    counter.increment().map_err(|error| error.to_string())
}
