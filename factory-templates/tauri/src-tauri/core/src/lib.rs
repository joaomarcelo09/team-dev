//! Domain logic for the Tauri template, kept independent of the window layer.
//!
//! The example stores a single counter in SQLite through rusqlite so the Tauri commands stay
//! thin and the behavior is unit tested here.

use std::fmt;
use std::path::Path;

use rusqlite::Connection;

/// Errors returned by the `app-core` crate.
#[derive(Debug)]
pub enum CoreError {
    /// The SQLite database returned an error.
    Database(rusqlite::Error),
}

impl fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Database(error) => write!(formatter, "database error: {error}"),
        }
    }
}

impl std::error::Error for CoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(error) => Some(error),
        }
    }
}

impl From<rusqlite::Error> for CoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}

/// Result alias for the `app-core` crate.
pub type Result<T> = std::result::Result<T, CoreError>;

/// A SQLite-backed counter, the template's single example domain type.
pub struct CounterStore {
    connection: Connection,
}

impl CounterStore {
    /// Opens the database at `path`, creating the schema when it does not exist.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::initialize(Connection::open(path)?)
    }

    /// Opens an in-memory database, which keeps tests independent of the filesystem.
    pub fn in_memory() -> Result<Self> {
        Self::initialize(Connection::open_in_memory()?)
    }

    /// Creates the schema and returns the store around the open connection.
    fn initialize(connection: Connection) -> Result<Self> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS counter (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                value INTEGER NOT NULL
            );
            INSERT OR IGNORE INTO counter (id, value) VALUES (1, 0);",
        )?;

        Ok(Self { connection })
    }

    /// Increments the counter and returns its new value.
    pub fn increment(&self) -> Result<i64> {
        self.connection.execute("UPDATE counter SET value = value + 1 WHERE id = 1", [])?;

        self.value()
    }

    /// Returns the current counter value.
    pub fn value(&self) -> Result<i64> {
        self.connection
            .query_row("SELECT value FROM counter WHERE id = 1", [], |row| row.get(0))
            .map_err(CoreError::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_zero() {
        let store = CounterStore::in_memory().expect("store opens");

        assert_eq!(store.value().expect("value reads"), 0);
    }

    #[test]
    fn increment_returns_the_new_value() {
        let store = CounterStore::in_memory().expect("store opens");

        assert_eq!(store.increment().expect("first increment"), 1);
        assert_eq!(store.increment().expect("second increment"), 2);
        assert_eq!(store.value().expect("value reads"), 2);
    }
}
