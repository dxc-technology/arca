//! Generic, type-aware metadata-backend migration (Phase 30, milestone M3).
//!
//! `migrate-db` copies every metadata table from one backend (SQLite or
//! PostgreSQL) to the other, in place. Blob files are filesystem-resident and
//! are NOT touched: only the metadata database changes. The operator then
//! switches `metadata_backend` in the config and restarts onto the new backend.
//!
//! The copier is a single generic column-walker driven by a static description
//! of every data table (its name + ordered columns + each column's logical
//! kind), rather than 24 hand-written row structs. Each backend implements a
//! pair of primitives (`dump_table` / `load_table`) that read/write rows as
//! `Vec<Cell>` according to the per-column [`ColumnKind`]; the orchestrator
//! [`migrate_all`] copies the tables in FK-safe order and reconciles row counts.
//!
//! The two schemas use identical table and column names; the only cross-backend
//! differences the kinds capture are:
//!   * SQLite stores RFC3339 timestamps as TEXT, PG as TIMESTAMPTZ → [`ColumnKind::Timestamp`]
//!   * SQLite stores booleans as INTEGER 0/1, PG as BOOLEAN → [`ColumnKind::Bool`]
//!   * SQLite stores JSON as TEXT, PG `objects.metadata`/`grants.document` as JSONB → [`ColumnKind::Json`]
//!   * a handful of PG columns are 32-bit `INTEGER` rather than `BIGINT` → [`ColumnKind::SmallInt`]

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::sqlite::SqliteStore;
#[cfg(feature = "postgres")]
use crate::pg::PgStore;

#[cfg(feature = "postgres")]
mod pg;
mod sqlite;

/// The logical type of a column, used to read/write its [`Cell`] consistently
/// across both backends despite their differing physical representations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    /// 64-bit integer (SQLite INTEGER, PG BIGINT).
    Int,
    /// 32-bit integer (PG `INTEGER` columns; SQLite still INTEGER).
    SmallInt,
    /// Floating point (SQLite REAL, PG DOUBLE PRECISION).
    Float,
    /// Free text / JSON-as-text (SQLite TEXT, PG TEXT).
    Text,
    /// JSON document (SQLite TEXT, PG JSONB).
    Json,
    /// Boolean (SQLite INTEGER 0/1, PG BOOLEAN).
    Bool,
    /// RFC3339 timestamp (SQLite TEXT, PG TIMESTAMPTZ).
    Timestamp,
}

/// A single cell value, backend-agnostic. NULL is explicit so nullable columns
/// round-trip faithfully.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Null,
    Int(i64),
    Float(f64),
    Text(String),
    /// A JSON value (stored as text in SQLite, JSONB in PG).
    Json(serde_json::Value),
    Bool(bool),
    Ts(DateTime<Utc>),
}

/// A column: its name and logical kind.
#[derive(Debug, Clone, Copy)]
pub struct Column {
    pub name: &'static str,
    pub kind: ColumnKind,
}

const fn col(name: &'static str, kind: ColumnKind) -> Column {
    Column { name, kind }
}

/// A table: its name and ordered columns. The `_migrations` table and the
/// transient `objects_new` are deliberately excluded.
#[derive(Debug, Clone, Copy)]
pub struct TableDesc {
    pub name: &'static str,
    pub columns: &'static [Column],
}

use ColumnKind::{Bool, Float, Int, Json, SmallInt, Text, Timestamp};

/// Every data table, in FK-safe load order (parents before children). The
/// column order is stable and identical across backends. Column kinds are
/// derived from the SQLite migrations (`sqlite/migrations.rs`) and the PG
/// migrations (`pg/migrations/*.sql`); see the module doc for the mapping.
pub const TABLES: &[TableDesc] = &[
    // --- Independent / parent tables ---
    TableDesc {
        name: "credentials",
        columns: &[
            col("access_key_id", Text),
            col("secret_access_key", Text),
            col("description", Text),
            col("created_at", Timestamp),
            col("active", Bool),
            col("user_id", Text),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "buckets",
        columns: &[
            col("name", Text),
            col("created_at", Timestamp),
            col("owner", Text),
        ],
    },
    TableDesc {
        name: "users",
        columns: &[
            col("user_id", Text),
            col("username", Text),
            col("description", Text),
            col("is_root", Bool),
            col("created_at", Timestamp),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "teams",
        columns: &[
            col("team_id", Text),
            col("name", Text),
            col("description", Text),
            col("created_at", Timestamp),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "grants",
        columns: &[
            col("grant_id", Text),
            col("name", Text),
            col("description", Text),
            col("document", Json),
            col("created_at", Timestamp),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "server_config",
        columns: &[
            col("config_key", Text),
            col("config_value", Text),
            col("updated_at", Timestamp),
        ],
    },
    // object_seq is a single-row counter; copied like any other table (one row).
    TableDesc {
        name: "object_seq",
        columns: &[col("value", Int)],
    },
    // --- Children of users/teams/grants ---
    TableDesc {
        name: "objects",
        columns: &[
            col("bucket", Text),
            col("key", Text),
            col("version_id", Text),
            col("blob_id", Text),
            col("size", Int),
            col("etag", Text),
            col("content_type", Text),
            col("last_modified", Timestamp),
            col("metadata", Json),
            col("encryption_algorithm", Text),
            col("encryption_key_id", Text),
            col("owner", Text),
            col("is_latest", Bool),
            col("is_delete_marker", Bool),
            col("retention_mode", Text),
            col("retain_until_date", Timestamp),
            col("legal_hold_status", Text),
            col("storage_class", Text),
            col("checksum_algorithm", Text),
            col("checksum_value", Text),
            col("replication_status", Text),
            col("seq", Int),
            col("is_tombstone", Bool),
            col("lock_updated_at", Timestamp),
            col("content_updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "multipart_uploads",
        columns: &[
            col("upload_id", Text),
            col("bucket", Text),
            col("key", Text),
            col("content_type", Text),
            col("initiated_at", Timestamp),
            col("metadata", Json),
            col("checksum_algorithm", Text),
        ],
    },
    TableDesc {
        name: "parts",
        columns: &[
            col("upload_id", Text),
            col("part_number", SmallInt),
            col("blob_id", Text),
            col("size", Int),
            col("etag", Text),
            col("checksum_value", Text),
            col("last_modified", Timestamp),
        ],
    },
    // --- Junction tables (after users/teams/grants) ---
    TableDesc {
        name: "user_grants",
        columns: &[
            col("user_id", Text),
            col("grant_id", Text),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "team_grants",
        columns: &[
            col("team_id", Text),
            col("grant_id", Text),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "team_members",
        columns: &[
            col("team_id", Text),
            col("user_id", Text),
            col("updated_at", Timestamp),
        ],
    },
    // --- Tag / config tables ---
    TableDesc {
        name: "object_tags",
        columns: &[
            col("bucket", Text),
            col("key", Text),
            col("version_id", Text),
            col("tag_key", Text),
            col("tag_value", Text),
        ],
    },
    TableDesc {
        name: "bucket_tags",
        columns: &[
            col("bucket", Text),
            col("tag_key", Text),
            col("tag_value", Text),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "bucket_config",
        columns: &[
            col("bucket", Text),
            col("config_key", Text),
            col("config_value", Text),
            col("updated_at", Timestamp),
        ],
    },
    // --- Log / event / journal tables ---
    TableDesc {
        name: "audit_log",
        columns: &[
            // The id is an autoincrement / bigserial surrogate; copy it verbatim
            // so references and ordering are preserved.
            col("id", Int),
            col("timestamp", Timestamp),
            col("request_id", Text),
            col("operation", Text),
            col("bucket", Text),
            col("key", Text),
            col("version_id", Text),
            col("user_id", Text),
            col("access_key_id", Text),
            col("source_ip", Text),
            col("http_method", Text),
            col("http_status", SmallInt),
            col("error_code", Text),
            col("bytes_sent", Int),
            col("bytes_received", Int),
            col("duration_ms", Int),
            col("user_agent", Text),
        ],
    },
    TableDesc {
        name: "metrics_snapshot",
        columns: &[
            col("id", Int),
            col("timestamp", Timestamp),
            col("bucket_count", SmallInt),
            col("object_count", Int),
            col("total_size_bytes", Int),
            col("disk_total_bytes", Int),
            col("disk_available_bytes", Int),
            col("active_connections", SmallInt),
        ],
    },
    TableDesc {
        name: "notification_events",
        columns: &[
            col("id", Text),
            col("bucket", Text),
            col("key", Text),
            col("event_name", Text),
            col("event_time", Timestamp),
            col("payload", Text),
            col("destination_url", Text),
            col("configuration_id", Text),
            col("delivery_status", Text),
            col("delivery_attempts", SmallInt),
            col("last_error", Text),
            col("created_at", Timestamp),
            col("connector_type", Text),
        ],
    },
    TableDesc {
        name: "presigned_urls",
        columns: &[
            col("id", Text),
            col("bucket", Text),
            col("key", Text),
            col("method", Text),
            col("expires_seconds", SmallInt),
            col("created_at", Timestamp),
            col("expires_at", Timestamp),
            col("access_key_id", Text),
        ],
    },
    TableDesc {
        name: "replication_journal",
        columns: &[
            col("id", Text),
            col("bucket", Text),
            col("key", Text),
            col("version_id", Text),
            col("rule_id", Text),
            col("event_type", Text),
            col("destination_endpoint", Text),
            col("destination_bucket", Text),
            col("status", Text),
            col("attempts", SmallInt),
            col("last_error", Text),
            col("next_retry_at", Timestamp),
            col("created_at", Timestamp),
            col("updated_at", Timestamp),
        ],
    },
    TableDesc {
        name: "control_tombstones",
        columns: &[
            col("entity_type", Text),
            col("entity_key", Text),
            col("deleted_at", Timestamp),
        ],
    },
    TableDesc {
        name: "maintenance_jobs",
        columns: &[
            col("id", Text),
            col("type", Text),
            col("status", Text),
            col("mode", Text),
            col("params", Text),
            col("total", Int),
            col("done", Int),
            col("rate", Float),
            col("last_error", Text),
            col("created_at", Timestamp),
            col("updated_at", Timestamp),
            col("started_at", Timestamp),
            col("finished_at", Timestamp),
        ],
    },
    TableDesc {
        name: "maintenance_job_logs",
        // Note: the SQLite schema has (job_id, ts, level, message); PG adds a
        // BIGSERIAL `seq` surrogate. We copy only the shared columns; PG's seq is
        // auto-assigned on insert (insert order matches dump order).
        columns: &[
            col("job_id", Text),
            col("ts", Timestamp),
            col("level", Text),
            col("message", Text),
        ],
    },
];

/// Number of rows inserted per batch for large tables.
pub const BATCH_SIZE: usize = 500;

/// A source/destination metadata backend behind a uniform read/write surface.
pub enum Backend {
    Sqlite(Arc<SqliteStore>),
    #[cfg(feature = "postgres")]
    Pg(Arc<PgStore>),
}

impl Backend {
    /// Human-readable backend name (matches the config `metadata_backend` value).
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Sqlite(_) => "sqlite",
            #[cfg(feature = "postgres")]
            Backend::Pg(_) => "postgres",
        }
    }

    /// Reads every row of `table` as ordered cells.
    pub async fn dump_table(&self, table: &TableDesc) -> Result<Vec<Vec<Cell>>, String> {
        match self {
            Backend::Sqlite(s) => sqlite::dump_table(s, table).await,
            #[cfg(feature = "postgres")]
            Backend::Pg(p) => pg::dump_table(p, table).await,
        }
    }

    /// Inserts `rows` into `table` (paged in [`BATCH_SIZE`] batches).
    pub async fn load_table(
        &self,
        table: &TableDesc,
        rows: &[Vec<Cell>],
    ) -> Result<(), String> {
        match self {
            Backend::Sqlite(s) => sqlite::load_table(s, table, rows).await,
            #[cfg(feature = "postgres")]
            Backend::Pg(p) => pg::load_table(p, table, rows).await,
        }
    }

    /// Number of rows currently in `table`.
    pub async fn count(&self, table: &str) -> Result<u64, String> {
        match self {
            Backend::Sqlite(s) => sqlite::count(s, table).await,
            #[cfg(feature = "postgres")]
            Backend::Pg(p) => pg::count(p, table).await,
        }
    }

    /// Removes every row from `table` (used by `--force`).
    pub async fn truncate(&self, table: &str) -> Result<(), String> {
        match self {
            Backend::Sqlite(s) => sqlite::truncate(s, table).await,
            #[cfg(feature = "postgres")]
            Backend::Pg(p) => pg::truncate(p, table).await,
        }
    }
}

/// Per-table copy result.
#[derive(Debug, Clone)]
pub struct TableReport {
    pub table: &'static str,
    pub source_count: u64,
    pub dest_count: u64,
}

/// Aggregate migration result.
#[derive(Debug, Clone, Default)]
pub struct MigrateReport {
    pub tables: Vec<TableReport>,
    pub total_rows: u64,
}

/// Tables whose presence signals real, operator-created data on the destination
/// (as opposed to the baseline rows every fresh schema auto-seeds: the root
/// user, the built-in grants and their attachment, the zero-valued seq counter).
/// A backend migration is a full replace, so it always truncates the
/// destination first; this set only governs the `!force` safety refusal.
const EVIDENCE_OF_USE: &[&str] = &[
    "buckets",
    "objects",
    "multipart_uploads",
    "credentials",
    "audit_log",
    "notification_events",
    "replication_journal",
    "presigned_urls",
];

/// Migrates every metadata table from `source` to `dest`.
///
/// A backend migration is a full replace: the destination is always truncated
/// (in reverse FK order) before copying. Unless `force` is set, it first refuses
/// if the destination shows evidence of existing operator data (see
/// [`EVIDENCE_OF_USE`]) — the always-present schema baseline (root user,
/// built-in grants, seq counter) is ignored so a freshly migrated/empty backend
/// is accepted. Tables are then copied in FK-safe order, reconciling
/// source/dest counts per table; a mismatch aborts with the offending table
/// named. `progress(idx, total, name)` is invoked before each table is copied.
pub async fn migrate_all<F>(
    source: &Backend,
    dest: &Backend,
    force: bool,
    mut progress: F,
) -> Result<MigrateReport, String>
where
    F: FnMut(usize, usize, &str),
{
    // 1. Safety check: refuse a destination that already holds real data.
    if !force {
        let mut non_empty = Vec::new();
        for t in EVIDENCE_OF_USE {
            if dest.count(t).await? > 0 {
                non_empty.push(*t);
            }
        }
        if !non_empty.is_empty() {
            return Err(format!(
                "destination backend ({}) already contains data: tables [{}] are non-empty. \
                 Re-run with force/--force to overwrite (the entire destination is replaced).",
                dest.name(),
                non_empty.join(", ")
            ));
        }
    }

    // 2. Always truncate the destination (full replace) in reverse FK order so
    //    children go before parents, then copy.
    for t in TABLES.iter().rev() {
        dest.truncate(t.name).await?;
    }

    // 3. Copy in FK-safe order.
    let total_tables = TABLES.len();
    let mut report = MigrateReport::default();
    for (idx, t) in TABLES.iter().enumerate() {
        progress(idx, total_tables, t.name);
        let rows = source.dump_table(t).await?;
        let source_count = rows.len() as u64;
        dest.load_table(t, &rows).await?;
        let dest_count = dest.count(t.name).await?;
        if dest_count != source_count {
            return Err(format!(
                "row-count mismatch for table \"{}\": source={source_count}, dest={dest_count}",
                t.name
            ));
        }
        report.total_rows += source_count;
        report.tables.push(TableReport {
            table: t.name,
            source_count,
            dest_count,
        });
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seeds a representative set of rows across several table shapes (text,
    /// timestamps, booleans, JSON, a tombstone, a junction table, the seq
    /// counter), migrates SQLite→SQLite via the generic copier, and verifies the
    /// counts plus a couple of spot-checked rows. This proves the column-walker
    /// without needing PostgreSQL.
    #[tokio::test]
    async fn sqlite_to_sqlite_round_trip() {
        use rusqlite::params;

        let src = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let dst = Arc::new(SqliteStore::open_in_memory().await.unwrap());

        let now = Utc::now().to_rfc3339();
        // Seed source directly via SQL so we exercise the raw column copier (the
        // typed store APIs would hide column-kind handling).
        let now2 = now.clone();
        src.write_conn().call(move |c| {
                // A credential (boolean active, timestamps).
                c.execute(
                    "INSERT INTO credentials (access_key_id, secret_access_key, description, created_at, active, user_id, updated_at) \
                     VALUES (?1, ?2, '', ?3, 1, 'root', ?3)",
                    params!["AKIA", "secret", now2],
                )?;
                // Two buckets.
                c.execute(
                    "INSERT INTO buckets (name, created_at, owner) VALUES ('b1', ?1, 'root')",
                    params![now2],
                )?;
                c.execute(
                    "INSERT INTO buckets (name, created_at, owner) VALUES ('b2', ?1, 'root')",
                    params![now2],
                )?;
                // A live object (JSON metadata, booleans, timestamp).
                c.execute(
                    "INSERT INTO objects (bucket, key, version_id, blob_id, size, etag, content_type, \
                       last_modified, metadata, owner, is_latest, is_delete_marker, storage_class, seq, is_tombstone) \
                     VALUES ('b1', 'k1', NULL, 'blob-1', 5, 'etag1', 'text/plain', ?1, ?2, 'root', 1, 0, 'STANDARD', 1, 0)",
                    params![now2, "{\"x\":\"y\"}"],
                )?;
                // A tombstone object (is_tombstone = 1, blob cleared).
                c.execute(
                    "INSERT INTO objects (bucket, key, version_id, blob_id, size, etag, content_type, \
                       last_modified, metadata, owner, is_latest, is_delete_marker, storage_class, seq, is_tombstone) \
                     VALUES ('b1', 'k2', 'v-2', '', 0, '', NULL, ?1, '{}', 'root', 0, 0, 'STANDARD', 2, 1)",
                    params![now2],
                )?;
                // A team + a team_members junction row.
                c.execute(
                    "INSERT INTO teams (team_id, name, description, created_at, updated_at) \
                     VALUES ('t1', 'team-one', '', ?1, ?1)",
                    params![now2],
                )?;
                c.execute(
                    "INSERT INTO team_members (team_id, user_id, updated_at) VALUES ('t1', 'root', ?1)",
                    params![now2],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .unwrap();

        // Bump the seq counter to a known value so it round-trips.
        src.write_conn().call(|c| {
                c.execute("UPDATE object_seq SET value = 2", [])?;
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .unwrap();

        let source = Backend::Sqlite(src.clone());
        let dest = Backend::Sqlite(dst.clone());

        let report = migrate_all(&source, &dest, false, |_, _, _| {})
            .await
            .expect("migration succeeds");

        // Every table reconciles.
        for t in &report.tables {
            assert_eq!(
                t.source_count, t.dest_count,
                "table {} count mismatch",
                t.table
            );
        }

        // Spot-check the destination: counts of the seeded tables.
        assert_eq!(dest.count("credentials").await.unwrap(), 1);
        assert_eq!(dest.count("buckets").await.unwrap(), 2);
        assert_eq!(dest.count("objects").await.unwrap(), 2);
        assert_eq!(dest.count("teams").await.unwrap(), 1);
        assert_eq!(dest.count("team_members").await.unwrap(), 1);
        assert_eq!(dest.count("object_seq").await.unwrap(), 1);
        // The migrated root user (seeded by the schema itself) survived.
        assert_eq!(dest.count("users").await.unwrap(), 1);

        // Spot-check a specific row's preserved values (JSON, booleans, tombstone).
        let (etag, meta, is_tombstone, is_latest): (String, String, i64, i64) = dst.write_conn().call(|c| {
                Ok::<_, rusqlite::Error>(c.query_row(
                    "SELECT etag, metadata, is_tombstone, is_latest FROM objects WHERE bucket='b1' AND key='k1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(etag, "etag1");
        assert_eq!(meta, "{\"x\":\"y\"}");
        assert_eq!(is_tombstone, 0);
        assert_eq!(is_latest, 1);

        let tomb: i64 = dst.write_conn().call(|c| {
                Ok::<_, rusqlite::Error>(c.query_row(
                    "SELECT is_tombstone FROM objects WHERE bucket='b1' AND key='k2'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(tomb, 1);

        // The seq counter value round-tripped.
        let seq_val: i64 = dst.write_conn().call(|c| Ok::<_, rusqlite::Error>(c.query_row("SELECT value FROM object_seq", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(seq_val, 2);
    }

    #[tokio::test]
    async fn refuses_destination_with_existing_data_without_force() {
        let src = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let dst = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        // Seed a bucket in the DESTINATION: that is evidence of operator data
        // (the schema-seeded baseline alone is ignored).
        let now = Utc::now().to_rfc3339();
        dst.write_conn()
            .call(move |c| {
                c.execute(
                    "INSERT INTO buckets (name, created_at, owner) VALUES ('exists', ?1, 'root')",
                    rusqlite::params![now],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .unwrap();
        let source = Backend::Sqlite(src);
        let dest = Backend::Sqlite(dst);
        let err = migrate_all(&source, &dest, false, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(err.contains("already contains data"), "got: {err}");
    }

    #[tokio::test]
    async fn fresh_destination_accepted_without_force() {
        // A pristine destination (only the schema-seeded baseline) is accepted
        // without --force, even though `users`/`grants`/`object_seq` are seeded.
        let src = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let dst = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let now = Utc::now().to_rfc3339();
        src.write_conn()
            .call(move |c| {
                c.execute(
                    "INSERT INTO buckets (name, created_at, owner) VALUES ('b', ?1, 'root')",
                    rusqlite::params![now],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .unwrap();
        let source = Backend::Sqlite(src);
        let dest = Backend::Sqlite(dst.clone());
        migrate_all(&source, &dest, false, |_, _, _| {})
            .await
            .expect("fresh destination accepted");
        assert_eq!(dest.count("buckets").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn force_truncates_then_copies() {
        let src = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let dst = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let now = Utc::now().to_rfc3339();
        let now2 = now.clone();
        src.write_conn().call(move |c| {
                c.execute(
                    "INSERT INTO buckets (name, created_at, owner) VALUES ('only', ?1, 'root')",
                    rusqlite::params![now2],
                )?;
                Ok::<(), rusqlite::Error>(())
            })
            .await
            .unwrap();

        let source = Backend::Sqlite(src);
        let dest = Backend::Sqlite(dst.clone());
        // Force succeeds despite the pre-seeded root user on the destination.
        let report = migrate_all(&source, &dest, true, |_, _, _| {})
            .await
            .expect("forced migration succeeds");
        for t in &report.tables {
            assert_eq!(t.source_count, t.dest_count, "table {}", t.table);
        }
        assert_eq!(dest.count("buckets").await.unwrap(), 1);
    }
}
