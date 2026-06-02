//! SQLite-backed storage for Arca.
//!
//! Uses `tokio-rusqlite` to run SQLite queries on a background thread,
//! keeping the async runtime non-blocking. The database runs in WAL mode
//! for concurrent read performance.
//!
//! A pool of read-only connections (default 20) handles SELECT queries in
//! parallel, while a single write connection serializes mutations. This
//! exploits WAL mode's concurrent reader capability.

mod audit;
mod control_snapshot;
mod control_tombstone;
mod credential;
mod grant;
mod metadata;
mod metrics;
mod migrations;
mod notification;
mod presigned_url;
mod replication;
mod server_config;
mod team;
pub(crate) mod user;

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use arca_core::error::ArcaError;

/// Type alias for the tokio-rusqlite error with rusqlite::Error as the inner type.
pub(crate) type TrError = tokio_rusqlite::Error<rusqlite::Error>;

/// Default number of read-only connections in the pool.
const DEFAULT_READ_POOL_SIZE: usize = 20;

/// PRAGMAs applied to every connection (writer and readers).
const COMMON_PRAGMAS: &str = "\
    PRAGMA journal_mode=WAL;\
    PRAGMA synchronous=NORMAL;\
    PRAGMA cache_size=-64000;\
    PRAGMA mmap_size=268435456;\
    PRAGMA temp_store=MEMORY;\
    PRAGMA busy_timeout=5000;";

/// Async wrapper around a SQLite database with a read pool.
pub struct SqliteStore {
    /// Single write connection (also used for reads when no pool is available).
    conn: tokio_rusqlite::Connection,
    /// Pool of read-only connections for parallel SELECT queries.
    read_pool: Vec<tokio_rusqlite::Connection>,
    /// Round-robin counter for read pool dispatch.
    read_idx: AtomicUsize,
    /// When true (clustered deployments), a hard delete leaves a tombstone row
    /// instead of removing it, so the deletion converges across nodes and is not
    /// resurrected by anti-entropy. Single-node deployments keep `false` and
    /// delete outright. Set once at startup via [`SqliteStore::set_cluster_mode`].
    cluster_mode: AtomicBool,
}

impl SqliteStore {
    /// Opens (or creates) a SQLite database at the given path.
    ///
    /// Enables WAL mode, applies pending migrations on the write connection,
    /// then opens a pool of read-only connections.
    pub async fn open(path: &Path) -> Result<Self, ArcaError> {
        let conn = tokio_rusqlite::Connection::open(path)
            .await
            .map_err(|e| ArcaError::Internal(format!("opening database: {e}")))?;

        conn.call(|conn| {
            conn.execute_batch(COMMON_PRAGMAS)?;
            migrations::run_migrations(conn)?;
            Ok(())
        })
        .await
        .map_err(|e: TrError| ArcaError::Internal(format!("initializing database: {e}")))?;

        // Open read-only connection pool.
        let mut read_pool = Vec::with_capacity(DEFAULT_READ_POOL_SIZE);
        for _ in 0..DEFAULT_READ_POOL_SIZE {
            let rc = tokio_rusqlite::Connection::open(path)
                .await
                .map_err(|e| ArcaError::Internal(format!("opening read connection: {e}")))?;
            rc.call(|conn| {
                conn.execute_batch(&format!(
                    "{COMMON_PRAGMAS}PRAGMA query_only=ON;"
                ))?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("configuring read connection: {e}")))?;
            read_pool.push(rc);
        }

        tracing::info!(
            path = %path.display(),
            read_pool_size = DEFAULT_READ_POOL_SIZE,
            "SQLite database ready"
        );
        Ok(Self { conn, read_pool, read_idx: AtomicUsize::new(0), cluster_mode: AtomicBool::new(false) })
    }

    /// Opens an in-memory SQLite database (for tests).
    ///
    /// In-memory databases cannot share data across connections, so the read
    /// pool is empty and all queries go through the single write connection.
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

        Ok(Self {
            conn,
            read_pool: Vec::new(),
            read_idx: AtomicUsize::new(0),
            cluster_mode: AtomicBool::new(false),
        })
    }

    /// Enables cluster mode: hard deletes leave tombstone rows instead of
    /// removing them (so deletions converge across nodes without resurrection).
    /// Called once at startup when `[cluster].enabled`. No-op on single node.
    pub fn set_cluster_mode(&self, on: bool) {
        self.cluster_mode.store(on, Ordering::Relaxed);
    }

    /// Whether hard deletes should tombstone (cluster mode) rather than remove.
    pub(crate) fn cluster_mode(&self) -> bool {
        self.cluster_mode.load(Ordering::Relaxed)
    }

    /// Dispatches a read-only query to the pool (round-robin).
    /// Falls back to the write connection if the pool is empty (in-memory tests).
    pub(crate) fn read_conn(&self) -> &tokio_rusqlite::Connection {
        if self.read_pool.is_empty() {
            return &self.conn;
        }
        let idx = self.read_idx.fetch_add(1, Ordering::Relaxed) % self.read_pool.len();
        &self.read_pool[idx]
    }
}
