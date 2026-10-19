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
mod maintenance;
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

/// Prepared statements each connection keeps. rusqlite's default (16) is
/// smaller than the writer's working set, so the hot statements would be
/// evicted and re-parsed on every request.
const STATEMENT_CACHE_CAPACITY: usize = 256;

/// `execute` and `query_row` through the connection's prepared-statement
/// cache. The request paths run the same SQL over and over, and parsing it
/// again on every call was a sizeable share of a small request's CPU. Only
/// stable SQL text belongs here: values go in bound parameters, never in the
/// string, or every distinct value would take a cache slot of its own.
pub(crate) trait CachedSql {
    fn execute_cached<P: rusqlite::Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize>;

    fn query_row_cached<T, P, F>(&self, sql: &str, params: P, f: F) -> rusqlite::Result<T>
    where
        P: rusqlite::Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>;
}

impl CachedSql for rusqlite::Connection {
    fn execute_cached<P: rusqlite::Params>(&self, sql: &str, params: P) -> rusqlite::Result<usize> {
        self.prepare_cached(sql)?.execute(params)
    }

    fn query_row_cached<T, P, F>(&self, sql: &str, params: P, f: F) -> rusqlite::Result<T>
    where
        P: rusqlite::Params,
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
    {
        self.prepare_cached(sql)?.query_row(params, f)
    }
}

/// PRAGMAs applied to every connection (writer and readers), followed by the
/// `synchronous` level chosen by [`synchronous_pragma`].
const COMMON_PRAGMAS: &str = "\
    PRAGMA journal_mode=WAL;\
    PRAGMA cache_size=-64000;\
    PRAGMA mmap_size=268435456;\
    PRAGMA temp_store=MEMORY;\
    PRAGMA busy_timeout=5000;";

/// `PRAGMA synchronous` for `[storage] fsync`. FULL fsyncs the WAL at every
/// commit, so an acknowledged write survives a power loss; NORMAL (WAL mode)
/// fsyncs only at checkpoints, so the last commits can be lost (the database
/// itself stays consistent).
// TECHDEBT(TD-048): with FULL every commit on the single writer connection is
// an fsync, so a slow disk caps the commit rate; group commit would share one
// fsync among concurrent writes.
fn synchronous_pragma(fsync: bool) -> &'static str {
    if fsync {
        "PRAGMA synchronous=FULL;"
    } else {
        "PRAGMA synchronous=NORMAL;"
    }
}

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
    /// Opens (or creates) a SQLite database at the given path, durably
    /// (`synchronous=FULL`). The offline tools use this.
    pub async fn open(path: &Path) -> Result<Self, ArcaError> {
        Self::open_with_fsync(path, true).await
    }

    /// Opens (or creates) a SQLite database at the given path, with commits
    /// fsynced or not according to `[storage] fsync`.
    ///
    /// Enables WAL mode, applies pending migrations on the write connection,
    /// then opens a pool of read-only connections.
    pub async fn open_with_fsync(path: &Path, fsync: bool) -> Result<Self, ArcaError> {
        let pragmas = format!("{COMMON_PRAGMAS}{}", synchronous_pragma(fsync));
        let conn = tokio_rusqlite::Connection::open(path)
            .await
            .map_err(|e| ArcaError::Internal(format!("opening database: {e}")))?;

        let writer_pragmas = pragmas.clone();
        conn.call(move |conn| {
            conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
            conn.execute_batch(&writer_pragmas)?;
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
            let reader_pragmas = format!("{pragmas}PRAGMA query_only=ON;");
            rc.call(move |conn| {
                conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
                conn.execute_batch(&reader_pragmas)?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("configuring read connection: {e}")))?;
            read_pool.push(rc);
        }

        tracing::info!(
            path = %path.display(),
            read_pool_size = DEFAULT_READ_POOL_SIZE,
            fsync,
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
            conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
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

    /// Runs `VACUUM` to rebuild the database file, reclaiming the pages freed by
    /// a bulk delete (e.g. `arca migrate-topology --to-single` purging every
    /// tombstone). Must run outside a transaction; this offline tool holds the
    /// only connection, so there is nothing to contend with.
    pub async fn vacuum(&self) -> Result<(), ArcaError> {
        self.conn
            .call(|conn| {
                conn.execute_batch("VACUUM")?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("vacuum: {e}")))
    }

    /// The single write connection (serializes mutations). Used by the
    /// backend-migration copier ([`crate::migration`]) for batched INSERTs and
    /// by tests that seed/inspect rows directly.
    pub(crate) fn write_conn(&self) -> &tokio_rusqlite::Connection {
        &self.conn
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `PRAGMA synchronous` of a connection: 1 = NORMAL, 2 = FULL.
    async fn synchronous(conn: &tokio_rusqlite::Connection) -> i64 {
        conn.call(|c| Ok(c.query_row("PRAGMA synchronous", [], |r| r.get(0))?))
            .await
            .map_err(|e: TrError| e)
            .unwrap()
    }

    #[tokio::test]
    async fn open_is_durable_by_default() {
        // Offline tools use `open`: they always commit with FULL.
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("arca.db")).await.unwrap();
        assert_eq!(synchronous(store.write_conn()).await, 2);
    }

    #[tokio::test]
    async fn fsync_on_commits_with_full() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open_with_fsync(&dir.path().join("arca.db"), true).await.unwrap();
        assert_eq!(synchronous(store.write_conn()).await, 2);
        assert_eq!(synchronous(store.read_conn()).await, 2);
    }

    #[tokio::test]
    async fn fsync_off_commits_with_normal() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open_with_fsync(&dir.path().join("arca.db"), false).await.unwrap();
        assert_eq!(synchronous(store.write_conn()).await, 1);
        assert_eq!(synchronous(store.read_conn()).await, 1);
    }

    /// Prepared statements currently alive on `conn`: cached statements stay
    /// prepared between calls, uncached ones are finalized after each call.
    fn live_statements(conn: &rusqlite::Connection) -> usize {
        let mut count = 0;
        // SAFETY: walks the connection's own statement list without touching
        // the statements; nothing else uses `conn` meanwhile.
        unsafe {
            let db = conn.handle();
            let mut stmt = rusqlite::ffi::sqlite3_next_stmt(db, std::ptr::null_mut());
            while !stmt.is_null() {
                count += 1;
                stmt = rusqlite::ffi::sqlite3_next_stmt(db, stmt);
            }
        }
        count
    }

    #[test]
    fn cached_helpers_reuse_one_prepared_statement_per_sql() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
        for v in 0..3 {
            assert_eq!(conn.execute_cached("INSERT INTO t (v) VALUES (?1)", [v]).unwrap(), 1);
        }
        for _ in 0..3 {
            let n: i64 = conn
                .query_row_cached("SELECT COUNT(*) FROM t WHERE v >= ?1", [0], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 3);
        }
        assert_eq!(live_statements(&conn), 2, "one cached statement per distinct SQL");
    }

    #[test]
    fn query_row_cached_reports_no_rows_like_query_row() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (v INTEGER)").unwrap();
        let err = conn
            .query_row_cached("SELECT v FROM t", [], |r| r.get::<_, i64>(0))
            .unwrap_err();
        assert!(matches!(err, rusqlite::Error::QueryReturnedNoRows));
    }

    /// rusqlite keeps 16 statements per connection by default, fewer than the
    /// writer's working set: the store's connections must keep many more, or
    /// the hot statements would be evicted and re-parsed on every request.
    #[tokio::test]
    async fn store_connections_cache_more_statements_than_the_rusqlite_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open(&dir.path().join("arca.db")).await.unwrap();
        for conn in [store.write_conn(), store.read_conn()] {
            let live = conn
                .call(|c| {
                    for i in 0..64 {
                        let _: i64 = c.query_row_cached(&format!("SELECT {i}"), [], |r| r.get(0))?;
                    }
                    Ok(live_statements(c))
                })
                .await
                .map_err(|e: TrError| e)
                .unwrap();
            assert_eq!(live, 64);
        }
    }
}
