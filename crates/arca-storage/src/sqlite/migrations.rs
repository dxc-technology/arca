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
    Migration {
        version: 7,
        description: "Add encryption columns to objects and create bucket_config table",
        sql: "ALTER TABLE objects ADD COLUMN encryption_algorithm TEXT;
              ALTER TABLE objects ADD COLUMN encryption_key_id TEXT;
              CREATE TABLE bucket_config (
                  bucket       TEXT NOT NULL,
                  config_key   TEXT NOT NULL,
                  config_value TEXT NOT NULL,
                  updated_at   TEXT NOT NULL,
                  PRIMARY KEY (bucket, config_key)
              )",
    },
    Migration {
        version: 8,
        description: "Add RBAC: users, teams, grants, ownership",
        sql: "
            CREATE TABLE users (
                user_id     TEXT PRIMARY KEY NOT NULL,
                username    TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                is_root     INTEGER NOT NULL DEFAULT 0,
                created_at  TEXT NOT NULL
            );

            CREATE TABLE teams (
                team_id     TEXT PRIMARY KEY NOT NULL,
                name        TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL
            );

            CREATE TABLE team_members (
                team_id TEXT NOT NULL REFERENCES teams(team_id),
                user_id TEXT NOT NULL REFERENCES users(user_id),
                PRIMARY KEY (team_id, user_id)
            );

            CREATE TABLE grants (
                grant_id    TEXT PRIMARY KEY NOT NULL,
                name        TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                document    TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );

            CREATE TABLE user_grants (
                user_id  TEXT NOT NULL REFERENCES users(user_id),
                grant_id TEXT NOT NULL REFERENCES grants(grant_id),
                PRIMARY KEY (user_id, grant_id)
            );

            CREATE TABLE team_grants (
                team_id  TEXT NOT NULL REFERENCES teams(team_id),
                grant_id TEXT NOT NULL REFERENCES grants(grant_id),
                PRIMARY KEY (team_id, grant_id)
            );

            -- Root user (deterministic ID)
            INSERT INTO users (user_id, username, description, is_root, created_at)
            VALUES ('root', 'root', 'System root user', 1, strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));

            -- Built-in grants
            INSERT INTO grants (grant_id, name, description, document, created_at, updated_at)
            VALUES ('grant-administrator-access', 'AdministratorAccess',
                'Full access to all operations',
                '{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":[\"*\"],\"Resource\":[\"*\"]}]}',
                strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));

            INSERT INTO grants (grant_id, name, description, document, created_at, updated_at)
            VALUES ('grant-s3-full-access', 'S3FullAccess',
                'Full access to S3 operations',
                '{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":[\"s3:*\"],\"Resource\":[\"*\"]}]}',
                strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));

            INSERT INTO grants (grant_id, name, description, document, created_at, updated_at)
            VALUES ('grant-s3-read-only', 'S3ReadOnlyAccess',
                'Read-only access to S3 operations',
                '{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Allow\",\"Action\":[\"s3:GetObject\",\"s3:ListBucket\",\"s3:ListAllMyBuckets\",\"s3:GetBucketLocation\",\"s3:GetBucketEncryption\"],\"Resource\":[\"*\"]}]}',
                strftime('%Y-%m-%dT%H:%M:%SZ', 'now'), strftime('%Y-%m-%dT%H:%M:%SZ', 'now'));

            -- Attach AdministratorAccess to root user
            INSERT INTO user_grants (user_id, grant_id)
            VALUES ('root', 'grant-administrator-access');

            -- Ownership tracking
            ALTER TABLE buckets ADD COLUMN owner TEXT NOT NULL DEFAULT 'root';
            ALTER TABLE objects ADD COLUMN owner TEXT NOT NULL DEFAULT 'root';

            -- Link credentials to users (existing creds become root's)
            ALTER TABLE credentials ADD COLUMN user_id TEXT NOT NULL DEFAULT 'root';
        ",
    },
    Migration {
        version: 9,
        description: "Add object versioning support",
        sql: "
            -- Recreate objects table with versioning columns.
            -- SQLite cannot ALTER TABLE to drop a primary key, so we recreate.
            CREATE TABLE objects_new (
                bucket               TEXT NOT NULL,
                key                  TEXT NOT NULL,
                version_id           TEXT,
                blob_id              TEXT NOT NULL DEFAULT '',
                size                 INTEGER NOT NULL DEFAULT 0,
                etag                 TEXT NOT NULL DEFAULT '',
                content_type         TEXT,
                last_modified        TEXT NOT NULL,
                metadata             TEXT NOT NULL DEFAULT '{}',
                encryption_algorithm TEXT,
                encryption_key_id    TEXT,
                owner                TEXT NOT NULL DEFAULT 'root',
                is_latest            INTEGER NOT NULL DEFAULT 1,
                is_delete_marker     INTEGER NOT NULL DEFAULT 0
            );

            INSERT INTO objects_new (bucket, key, version_id, blob_id, size, etag,
                content_type, last_modified, metadata, encryption_algorithm,
                encryption_key_id, owner, is_latest, is_delete_marker)
            SELECT bucket, key, NULL, blob_id, size, etag, content_type,
                last_modified, metadata, encryption_algorithm, encryption_key_id,
                owner, 1, 0
            FROM objects;

            DROP TABLE objects;
            ALTER TABLE objects_new RENAME TO objects;

            -- Enforce exactly one is_latest=1 row per (bucket, key).
            CREATE UNIQUE INDEX idx_objects_latest
                ON objects(bucket, key) WHERE is_latest = 1;

            -- Fast version listing by key.
            CREATE INDEX idx_objects_versions
                ON objects(bucket, key, last_modified DESC);
        ",
    },
    Migration {
        version: 10,
        description: "Add server_config, audit_log, and metrics_snapshot tables",
        sql: "
            CREATE TABLE server_config (
                config_key   TEXT PRIMARY KEY NOT NULL,
                config_value TEXT NOT NULL,
                updated_at   TEXT NOT NULL
            );

            CREATE TABLE audit_log (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp       TEXT NOT NULL,
                request_id      TEXT NOT NULL,
                operation       TEXT NOT NULL,
                bucket          TEXT,
                key             TEXT,
                version_id      TEXT,
                user_id         TEXT,
                access_key_id   TEXT,
                source_ip       TEXT,
                http_method     TEXT NOT NULL,
                http_status     INTEGER NOT NULL,
                error_code      TEXT,
                bytes_sent      INTEGER NOT NULL DEFAULT 0,
                bytes_received  INTEGER NOT NULL DEFAULT 0,
                duration_ms     INTEGER NOT NULL DEFAULT 0,
                user_agent      TEXT
            );

            CREATE INDEX idx_audit_log_timestamp ON audit_log(timestamp);
            CREATE INDEX idx_audit_log_bucket ON audit_log(bucket, timestamp);
            CREATE INDEX idx_audit_log_user ON audit_log(user_id, timestamp);
            CREATE INDEX idx_audit_log_operation ON audit_log(operation, timestamp);

            CREATE TABLE metrics_snapshot (
                id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                timestamp            TEXT NOT NULL,
                bucket_count         INTEGER NOT NULL,
                object_count         INTEGER NOT NULL,
                total_size_bytes     INTEGER NOT NULL,
                disk_total_bytes     INTEGER,
                disk_available_bytes INTEGER,
                active_connections   INTEGER NOT NULL DEFAULT 0
            );

            CREATE INDEX idx_metrics_snapshot_timestamp ON metrics_snapshot(timestamp);
        ",
    },
    Migration {
        version: 11,
        description: "Add object_tags and bucket_tags tables",
        sql: "
            CREATE TABLE object_tags (
                bucket     TEXT NOT NULL,
                key        TEXT NOT NULL,
                version_id TEXT NOT NULL DEFAULT '',
                tag_key    TEXT NOT NULL,
                tag_value  TEXT NOT NULL,
                PRIMARY KEY (bucket, key, version_id, tag_key)
            );

            CREATE TABLE bucket_tags (
                bucket    TEXT NOT NULL,
                tag_key   TEXT NOT NULL,
                tag_value TEXT NOT NULL,
                PRIMARY KEY (bucket, tag_key)
            );
        ",
    },
    Migration {
        version: 12,
        description: "Add Object Lock columns to objects table",
        sql: "
            ALTER TABLE objects ADD COLUMN retention_mode TEXT;
            ALTER TABLE objects ADD COLUMN retain_until_date TEXT;
            ALTER TABLE objects ADD COLUMN legal_hold_status TEXT;
        ",
    },
    Migration {
        version: 13,
        description: "Add storage_class and checksum columns; add checksum and last_modified to parts; add checksum_algorithm to multipart_uploads",
        sql: "
            ALTER TABLE objects ADD COLUMN storage_class TEXT NOT NULL DEFAULT 'STANDARD';
            ALTER TABLE objects ADD COLUMN checksum_algorithm TEXT;
            ALTER TABLE objects ADD COLUMN checksum_value TEXT;
            ALTER TABLE parts ADD COLUMN checksum_value TEXT;
            ALTER TABLE parts ADD COLUMN last_modified TEXT;
            ALTER TABLE multipart_uploads ADD COLUMN checksum_algorithm TEXT;
        ",
    },
    Migration {
        version: 14,
        description: "Add notification_events table for event log and delivery tracking",
        sql: "
            CREATE TABLE IF NOT EXISTS notification_events (
                id                TEXT PRIMARY KEY,
                bucket            TEXT NOT NULL,
                key               TEXT NOT NULL,
                event_name        TEXT NOT NULL,
                event_time        TEXT NOT NULL,
                payload           TEXT NOT NULL,
                destination_url   TEXT NOT NULL,
                configuration_id  TEXT NOT NULL,
                delivery_status   TEXT NOT NULL DEFAULT 'pending',
                delivery_attempts INTEGER NOT NULL DEFAULT 0,
                last_error        TEXT,
                created_at        TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_notification_events_bucket ON notification_events(bucket);
            CREATE INDEX IF NOT EXISTS idx_notification_events_status ON notification_events(delivery_status);
            CREATE INDEX IF NOT EXISTS idx_notification_events_created ON notification_events(created_at);
        ",
    },
    Migration {
        version: 15,
        description: "Add connector_type column to notification_events for modular connector support",
        sql: "
            ALTER TABLE notification_events ADD COLUMN connector_type TEXT NOT NULL DEFAULT 'webhook';
        ",
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
        assert_eq!(version, 15);

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

        // Verify notification_events table exists
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM notification_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn migrations_idempotent() {
        let conn = open_memory_db();
        run_migrations(&conn).unwrap();
        run_migrations(&conn).unwrap();

        let version = current_version(&conn).unwrap();
        assert_eq!(version, 15);

        // Fifteen migration records
        let count: u32 = conn
            .query_row("SELECT COUNT(*) FROM _migrations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 15);
    }
}
