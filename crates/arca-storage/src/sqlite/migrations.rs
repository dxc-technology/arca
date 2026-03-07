//! Version-tracked SQLite migration runner.
//!
//! Migrations are stored in a `_migrations` table. Each migration has a version
//! number and a description. The runner applies pending migrations in order and
//! records their completion.

use rusqlite::{params, Connection};

/// A single migration step.
struct Migration {
    version: u32,
    description: &'static str,
    sql: &'static str,
}

/// All known migrations, in version order.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "Create credentials table",
        sql: "CREATE TABLE credentials (
            access_key_id     TEXT PRIMARY KEY NOT NULL,
            secret_access_key TEXT NOT NULL,
            description       TEXT NOT NULL DEFAULT '',
            created_at        TEXT NOT NULL,
            active            INTEGER NOT NULL DEFAULT 1
        )",
    },
    Migration {
        version: 2,
        description: "Create buckets table",
        sql: "CREATE TABLE buckets (
            name       TEXT PRIMARY KEY NOT NULL,
            created_at TEXT NOT NULL
        )",
    },
    Migration {
        version: 3,
        description: "Create objects table",
        sql: "CREATE TABLE objects (
            bucket        TEXT NOT NULL,
            key           TEXT NOT NULL,
            blob_id       TEXT NOT NULL,
            size          INTEGER NOT NULL,
            etag          TEXT NOT NULL,
            content_type  TEXT,
            last_modified TEXT NOT NULL,
            PRIMARY KEY (bucket, key)
        )",
    },
    Migration {
        version: 4,
        description: "Create multipart upload tables",
        sql: "CREATE TABLE multipart_uploads (
            upload_id    TEXT PRIMARY KEY NOT NULL,
            bucket       TEXT NOT NULL,
            key          TEXT NOT NULL,
            content_type TEXT,
            initiated_at TEXT NOT NULL
        );
        CREATE TABLE parts (
            upload_id   TEXT NOT NULL,
            part_number INTEGER NOT NULL,
            blob_id     TEXT NOT NULL,
            size        INTEGER NOT NULL,
            etag        TEXT NOT NULL,
            PRIMARY KEY (upload_id, part_number)
        )",
    },
    Migration {
        version: 5,
        description: "Add admin flag to credentials",
        // Existing credentials are promoted to admin (they had full access before);
        // new credentials created via API default to non-admin (INSERT sets explicitly).
        sql: "ALTER TABLE credentials ADD COLUMN admin INTEGER NOT NULL DEFAULT 0;
              UPDATE credentials SET admin = 1",
    },
    Migration {
        version: 6,
        description: "Add metadata column to objects and multipart_uploads",
        // JSON-encoded HashMap<String, String>. Defaults to '{}' for existing rows.
        sql: "ALTER TABLE objects ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}';
              ALTER TABLE multipart_uploads ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'",
    },
];

/// Ensures the `_migrations` tracking table exists.
fn ensure_migrations_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version     INTEGER PRIMARY KEY NOT NULL,
            description TEXT NOT NULL,
            applied_at  TEXT NOT NULL
        )",
    )
}

/// Returns the current (highest applied) migration version, or 0 if none.
fn current_version(conn: &Connection) -> rusqlite::Result<u32> {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _migrations",
        [],
        |row| row.get(0),
    )
}

/// Applies all pending migrations to the database.
///
/// This function is meant to be called on a synchronous `rusqlite::Connection`
/// (inside `tokio_rusqlite::Connection::call`).
pub fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    ensure_migrations_table(conn)?;
    let current = current_version(conn)?;

    for migration in MIGRATIONS {
        if migration.version <= current {
            continue;
        }

        tracing::info!(
            version = migration.version,
            description = migration.description,
            "Applying migration"
        );

        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(migration.sql)?;
        tx.execute(
            "INSERT INTO _migrations (version, description, applied_at) VALUES (?1, ?2, ?3)",
            params![
                migration.version,
                migration.description,
                chrono::Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        conn
    }

    #[test]
    fn migrations_on_fresh_db() {
        let conn = open_memory_db();
        run_migrations(&conn).unwrap();

        let version = current_version(&conn).unwrap();
        assert_eq!(version, 6);

        // Verify credentials table exists
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM credentials", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify buckets table exists
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM buckets", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify objects table exists
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify multipart_uploads table exists
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM multipart_uploads", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);

        // Verify parts table exists
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM parts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn migrations_idempotent() {
        let conn = open_memory_db();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let version = current_version(&conn).unwrap();
        assert_eq!(version, 6);

        // Six migration records
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 6);
    }
}
