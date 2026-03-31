//! PostgreSQL-backed storage for Arca.
//!
//! Uses `sqlx-core` / `sqlx-postgres` with an async connection pool for concurrent access.
//! Schema migrations are applied automatically at startup.

mod audit;
mod credential;
mod grant;
mod metadata;
mod metrics;
mod server_config;
mod team;
pub(crate) mod user;

use arca_core::error::ArcaError;
use sqlx_core::row::Row;

/// Async wrapper around a PostgreSQL connection pool.
pub struct PgStore {
    pool: sqlx_postgres::PgPool,
}

/// A single migration step.
struct Migration {
    version: i32,
    description: &'static str,
    sql: &'static str,
}

/// All known PostgreSQL migrations.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "Initial schema (equivalent to SQLite v1-v13)",
        sql: include_str!("migrations/0001_initial_schema.sql"),
    },
];

impl PgStore {
    /// Connects to PostgreSQL and applies pending migrations.
    pub async fn open(connection_string: &str, max_connections: u32) -> Result<Self, ArcaError> {
        let pool = sqlx_postgres::PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(connection_string)
            .await
            .map_err(|e| ArcaError::Internal(format!("connecting to PostgreSQL: {e}")))?;

        run_migrations(&pool).await?;

        tracing::info!("PostgreSQL database ready");
        Ok(Self { pool })
    }
}

/// Ensures the `_migrations` tracking table exists.
async fn ensure_migrations_table(pool: &sqlx_postgres::PgPool) -> Result<(), ArcaError> {
    sqlx_core::query::query(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version     INTEGER PRIMARY KEY NOT NULL,
            description TEXT NOT NULL,
            applied_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| ArcaError::Internal(format!("creating migrations table: {e}")))?;
    Ok(())
}

/// Returns the current (highest applied) migration version, or 0 if none.
async fn current_version(pool: &sqlx_postgres::PgPool) -> Result<i32, ArcaError> {
    let row = sqlx_core::query::query(
        "SELECT COALESCE(MAX(version), 0) FROM _migrations",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| ArcaError::Internal(format!("reading migration version: {e}")))?;
    let version: i32 = row.get(0);
    Ok(version)
}

/// Applies all pending migrations.
async fn run_migrations(pool: &sqlx_postgres::PgPool) -> Result<(), ArcaError> {
    ensure_migrations_table(pool).await?;
    let current = current_version(pool).await?;

    for migration in MIGRATIONS {
        if migration.version <= current {
            continue;
        }

        tracing::info!(
            version = migration.version,
            description = migration.description,
            "Applying PostgreSQL migration"
        );

        // Run migration + record in a transaction.
        let mut tx = pool.begin().await
            .map_err(|e| ArcaError::Internal(format!("begin migration tx: {e}")))?;

        // Use raw_sql for multi-statement migrations (query() only supports single statements).
        sqlx_core::raw_sql::raw_sql(migration.sql)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("migration v{}: {e}", migration.version)))?;

        sqlx_core::query::query(
            "INSERT INTO _migrations (version, description) VALUES ($1, $2)",
        )
        .bind(migration.version)
        .bind(migration.description)
        .execute(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("recording migration v{}: {e}", migration.version)))?;

        tx.commit().await
            .map_err(|e| ArcaError::Internal(format!("commit migration v{}: {e}", migration.version)))?;
    }

    Ok(())
}
