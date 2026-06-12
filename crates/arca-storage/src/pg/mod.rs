//! PostgreSQL-backed storage for Arca.
//!
//! Uses `sqlx-core` / `sqlx-postgres` with an async connection pool for concurrent access.
//! Schema migrations are applied automatically at startup.

mod audit;
mod control_snapshot;
mod control_tombstone;
mod credential;
mod grant;
mod metadata;
mod metrics;
mod notification;
mod presigned_url;
mod replication;
mod server_config;
mod team;
pub(crate) mod user;

use std::sync::atomic::{AtomicBool, Ordering};

use arca_core::error::ArcaError;
use sqlx_core::row::Row;

/// Async wrapper around a PostgreSQL connection pool.
pub struct PgStore {
    pool: sqlx_postgres::PgPool,
    /// When true (clustered deployments), a hard delete leaves a tombstone row
    /// instead of removing it (see [`crate::sqlite::SqliteStore`] for rationale).
    /// Set once at startup via [`PgStore::set_cluster_mode`].
    cluster_mode: AtomicBool,
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
    Migration {
        version: 2,
        description: "Add connector_type column to notification_events",
        sql: include_str!("migrations/0002_notification_connector_type.sql"),
    },
    Migration {
        version: 3,
        description: "Create presigned_urls tracking table",
        sql: include_str!("migrations/0003_presigned_urls.sql"),
    },
    Migration {
        version: 4,
        description: "Add replication journal and replication_status on objects",
        sql: include_str!("migrations/0004_replication.sql"),
    },
    Migration {
        version: 5,
        description: "Add node-local monotonic seq to objects (cluster anti-entropy changed-since cursor)",
        sql: include_str!("migrations/0005_cluster_seq.sql"),
    },
    Migration {
        version: 6,
        description: "Add is_tombstone to objects (cluster hard-delete convergence)",
        sql: include_str!("migrations/0006_tombstones.sql"),
    },
    Migration {
        version: 7,
        description: "Add updated_at to credentials/users/teams (cluster control-plane LWW reconcile)",
        sql: include_str!("migrations/0007_control_updated_at.sql"),
    },
    Migration {
        version: 8,
        description: "Add control_tombstones table (cluster control-plane delete convergence)",
        sql: include_str!("migrations/0008_control_tombstones.sql"),
    },
    Migration {
        version: 9,
        description: "Replace the objects_seq sequence with a commit-ordered object_seq counter (review §2.2)",
        sql: include_str!("migrations/0009_commit_ordered_seq.sql"),
    },
    Migration {
        version: 10,
        description: "Add updated_at to grant attachments, memberships and bucket_tags (R5 control reconcile)",
        sql: include_str!("migrations/0010_control_reconcile_families.sql"),
    },
    Migration {
        version: 11,
        description: "Add lock_updated_at to objects (N2: lock-state LWW dimension)",
        sql: include_str!("migrations/0011_lock_updated_at.sql"),
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
        Ok(Self { pool, cluster_mode: AtomicBool::new(false) })
    }

    /// Enables cluster mode: hard deletes leave tombstone rows instead of
    /// removing them. Called once at startup when `[cluster].enabled`.
    pub fn set_cluster_mode(&self, on: bool) {
        self.cluster_mode.store(on, Ordering::Relaxed);
    }

    /// Whether hard deletes should tombstone (cluster mode) rather than remove.
    pub(crate) fn cluster_mode(&self) -> bool {
        self.cluster_mode.load(Ordering::Relaxed)
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
