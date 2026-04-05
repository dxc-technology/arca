//! SQLite-backed storage for Arca.
//!
//! Uses `tokio-rusqlite` to run SQLite queries on a background thread,
//! keeping the async runtime non-blocking. The database runs in WAL mode
//! for concurrent read performance.

mod audit;
mod credential;
mod grant;
mod metadata;
mod metrics;
mod migrations;
mod notification;
mod presigned_url;
mod server_config;
mod team;
pub(crate) mod user;

use std::path::Path;

use arca_core::error::ArcaError;

/// Type alias for the tokio-rusqlite error with rusqlite::Error as the inner type.
pub(crate) type TrError = tokio_rusqlite::Error<rusqlite::Error>;

/// Async wrapper around a SQLite database.
pub struct SqliteStore {
    conn: tokio_rusqlite::Connection,
}

impl SqliteStore {
    /// Opens (or creates) a SQLite database at the given path.
    ///
    /// Enables WAL mode and applies pending migrations.
    pub async fn open(path: &Path) -> Result<Self, ArcaError> {
        let conn = tokio_rusqlite::Connection::open(path)
            .await
            .map_err(|e| ArcaError::Internal(format!("opening database: {e}")))?;

        conn.call(|conn| {
            conn.execute_batch("PRAGMA journal_mode=WAL")?;
            migrations::run_migrations(conn)?;
            Ok(())
        })
        .await
        .map_err(|e: TrError| ArcaError::Internal(format!("initializing database: {e}")))?;

        tracing::info!(path = %path.display(), "SQLite database ready");
        Ok(Self { conn })
    }

    /// Opens an in-memory SQLite database (for tests).
    pub async fn open_in_memory() -> Result<Self, ArcaError> {
        let conn = tokio_rusqlite::Connection::open_in_memory()
            .await
            .map_err(|e| ArcaError::Internal(format!("opening in-memory database: {e}")))?;

        conn.call(|conn| {
            conn.execute_batch("PRAGMA journal_mode=WAL")?;
            migrations::run_migrations(conn)?;
            Ok(())
        })
        .await
        .map_err(|e: TrError| ArcaError::Internal(format!("initializing in-memory database: {e}")))?;

        Ok(Self { conn })
    }
}
