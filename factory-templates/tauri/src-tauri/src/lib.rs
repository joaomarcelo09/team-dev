//! Tauri application: window setup, shared state and thin command handlers.

use std::sync::Mutex;

use app_core::CounterStore;
use tauri::Manager;

mod commands;

/// Shared state available to the Tauri commands.
pub(crate) struct AppState {
    /// SQLite-backed counter from the `app-core` crate.
    pub(crate) counter: Mutex<CounterStore>,
}

/// Builds and runs the desktop application.
pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;
            let counter = CounterStore::open(data_dir.join("counter.sqlite3"))?;
            app.manage(AppState { counter: Mutex::new(counter) });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![commands::increment_counter])
        .run(tauri::generate_context!())
        .expect("error while running the Tauri application");
}
