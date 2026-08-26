//! PostgreSQL implementation of the `MetadataStore` trait.

use arca_core::error::{ArcaError, S3Error};
use arca_core::store::{DeletePrecondition, MetadataStore, WritePrecondition};
use arca_core::types::{
    BlobId, BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats,
    VersioningState,
};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;
use std::collections::HashMap;

use super::PgStore;

/// Column list for all object SELECT queries (20 columns).
const OBJECT_COLUMNS: &str = "bucket, key, blob_id, size, etag, content_type, last_modified, \
    metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, \
    is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, \
    checksum_algorithm, checksum_value, replication_status, lock_updated_at, content_updated_at";

/// Reads the bucket versioning state from `bucket_config`.
/// Called inside a transaction context.
async fn get_versioning_state(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    bucket: &str,
) -> VersioningState {
    let row = sqlx_core::query::query(
        "SELECT config_value FROM bucket_config WHERE bucket = $1 AND config_key = 'versioning'",
    )
    .bind(bucket)
    .fetch_optional(&mut **tx)
    .await
    .ok()
    .flatten();

    match row {
        Some(ref r) => {
            let v: String = r.get("config_value");
            match v.as_str() {
                "Enabled" => VersioningState::Enabled,
                "Suspended" => VersioningState::Suspended,
                _ => VersioningState::Unversioned,
            }
        }
        None => VersioningState::Unversioned,
    }
}

/// Fetches the latest object version (the row with `is_latest=true`) for a given bucket/key.
async fn fetch_latest_object(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    bucket: &str,
    key: &str,
) -> Result<Option<ObjectRecord>, sqlx_core::error::Error> {
    let sql = format!(
        "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = $1 AND key = $2 AND is_latest = TRUE"
    );
    let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
        .bind(bucket)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.as_ref().map(row_to_object_record))
}

/// Fetches the current object exactly as `MetadataStore::get_object` would see
/// it: the latest version, with delete markers filtered out. Used by
/// `put_object_if` to evaluate `WritePrecondition` against what the
/// handler-side early check already saw (see plan §3.5).
async fn fetch_current_object(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    bucket: &str,
    key: &str,
) -> Result<Option<ObjectRecord>, sqlx_core::error::Error> {
    let sql = format!(
        "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = $1 AND key = $2 AND is_latest = TRUE AND is_delete_marker = FALSE"
    );
    let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
        .bind(bucket)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.as_ref().map(row_to_object_record))
}

/// Fetches the null-version row (version_id IS NULL) for a given bucket/key.
async fn fetch_null_version(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    bucket: &str,
    key: &str,
) -> Result<Option<ObjectRecord>, sqlx_core::error::Error> {
    let sql = format!(
        "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL AND is_tombstone = FALSE"
    );
    let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
        .bind(bucket)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.as_ref().map(row_to_object_record))
}

/// Returns the next node-local monotonic `seq`, advancing the single-row
/// `object_seq` counter INSIDE the caller's transaction (migration 0009).
///
/// The `UPDATE ... RETURNING` takes the counter's row lock until commit, which
/// serializes the assignment: seq order = commit order, so a peer's
/// changed-since manifest cursor can never skip a row still in flight (review
/// §2.2 — the previous SEQUENCE was not transactional and lost rows). Mirrors
/// the SQLite `next_object_seq`.
///
/// LOCK-ORDER RULE: every objects-writing transaction must call this BEFORE
/// its first row-mutating statement (uniform seq → rows order). Transactions
/// that never take a seq (plain single-node deletes) only lock rows and cannot
/// form a cycle with seq holders over the single counter resource.
async fn next_object_seq(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
) -> Result<i64, sqlx_core::error::Error> {
    let row = sqlx_core::query::query("UPDATE object_seq SET value = value + 1 RETURNING value")
        .fetch_one(&mut **tx)
        .await?;
    Ok(row.get::<i64, _>("value"))
}

/// Inserts a new object row into the `objects` table, stamping the given
/// node-local `seq` (from [`next_object_seq`], same transaction).
async fn insert_object_row(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    record: &ObjectRecord,
    metadata_json: &serde_json::Value,
    seq: i64,
) -> Result<(), sqlx_core::error::Error> {
    sqlx_core::query::query(
        "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, \
         metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, \
         is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, \
         checksum_algorithm, checksum_value, seq, lock_updated_at, content_updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23)",
    )
    .bind(&record.bucket)
    .bind(&record.key)
    .bind(&record.blob_id.0)
    .bind(record.size as i64)
    .bind(&record.etag)
    .bind(&record.content_type)
    .bind(record.last_modified)
    .bind(metadata_json)
    .bind(&record.encryption_algorithm)
    .bind(&record.encryption_key_id)
    .bind(&record.owner)
    .bind(&record.version_id)
    .bind(record.is_latest)
    .bind(record.is_delete_marker)
    .bind(&record.retention_mode)
    .bind(record.retain_until_date)
    .bind(&record.legal_hold_status)
    .bind(&record.storage_class)
    .bind(&record.checksum_algorithm)
    .bind(&record.checksum_value)
    .bind(seq)
    .bind(record.lock_updated_at)
    .bind(record.content_updated_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Inserts a replicated object row verbatim, including `replication_status`,
/// stamping the given node-local `seq` (from [`next_object_seq`], same
/// transaction). `is_latest` is forced to FALSE so the partial unique latest
/// index is never transiently violated; the caller then runs
/// [`recompute_is_latest`]. Mirrors the SQLite `insert_replicated_row` for
/// cross-backend convergence.
async fn insert_replicated_row(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    record: &ObjectRecord,
    metadata_json: &serde_json::Value,
    seq: i64,
) -> Result<(), sqlx_core::error::Error> {
    sqlx_core::query::query(
        "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, \
         metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, \
         is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, \
         checksum_algorithm, checksum_value, replication_status, is_tombstone, seq, lock_updated_at, \
         content_updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, FALSE, $13, $14, $15, $16, \
         $17, $18, $19, $20, $21, $22, $23, $24)",
    )
    .bind(&record.bucket)
    .bind(&record.key)
    .bind(&record.blob_id.0)
    .bind(record.size as i64)
    .bind(&record.etag)
    .bind(&record.content_type)
    .bind(record.last_modified)
    .bind(metadata_json)
    .bind(&record.encryption_algorithm)
    .bind(&record.encryption_key_id)
    .bind(&record.owner)
    .bind(&record.version_id)
    .bind(record.is_delete_marker)
    .bind(&record.retention_mode)
    .bind(record.retain_until_date)
    .bind(&record.legal_hold_status)
    .bind(&record.storage_class)
    .bind(&record.checksum_algorithm)
    .bind(&record.checksum_value)
    .bind(&record.replication_status)
    .bind(record.is_tombstone)
    .bind(seq)
    .bind(record.lock_updated_at)
    .bind(record.content_updated_at)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Recomputes `is_latest` for a `(bucket, key)` deterministically: clears the
/// flag on every version, then sets it on the single newest by
/// `(last_modified DESC, version_id DESC, blob_id DESC)`.
///
/// `version_id DESC NULLS LAST` is REQUIRED so a null-version row sorts last in
/// the version_id tiebreak, matching SQLite (where NULL is the smallest value,
/// hence last under DESC). Without `NULLS LAST` PostgreSQL would order NULLs
/// first and the two backends could disagree on the current version — a
/// divergence the cluster must never allow.
async fn recompute_is_latest(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    bucket: &str,
    key: &str,
) -> Result<(), sqlx_core::error::Error> {
    sqlx_core::query::query("UPDATE objects SET is_latest = FALSE WHERE bucket = $1 AND key = $2")
        .bind(bucket)
        .bind(key)
        .execute(&mut **tx)
        .await?;
    sqlx_core::query::query(
        "UPDATE objects SET is_latest = TRUE WHERE ctid = (
            SELECT ctid FROM objects WHERE bucket = $1 AND key = $2 AND is_tombstone = FALSE
            ORDER BY last_modified DESC, version_id DESC NULLS LAST, blob_id DESC
            LIMIT 1
        )",
    )
    .bind(bucket)
    .bind(key)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Escapes special characters in a LIKE pattern so they are matched literally.
/// PostgreSQL LIKE special characters are `%` and `_`. We use `\` as the escape character.
fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '%' | '_' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Converts a PostgreSQL row to an `ObjectRecord`.
fn row_to_object_record(row: &sqlx_postgres::PgRow) -> ObjectRecord {
    let metadata_json: serde_json::Value = row.get("metadata");
    let metadata: HashMap<String, String> =
        serde_json::from_value(metadata_json).unwrap_or_default();

    ObjectRecord {
        bucket: row.get("bucket"),
        key: row.get("key"),
        blob_id: BlobId(row.get("blob_id")),
        size: row.get::<i64, _>("size") as u64,
        etag: row.get("etag"),
        content_type: row.get("content_type"),
        last_modified: row.get("last_modified"),
        metadata,
        encryption_algorithm: row.get("encryption_algorithm"),
        encryption_key_id: row.get("encryption_key_id"),
        owner: row.get("owner"),
        version_id: row.get("version_id"),
        is_latest: row.get("is_latest"),
        is_delete_marker: row.get("is_delete_marker"),
        retention_mode: row.get("retention_mode"),
        retain_until_date: row.get("retain_until_date"),
        legal_hold_status: row.get("legal_hold_status"),
        storage_class: row.get("storage_class"),
        checksum_algorithm: row.get("checksum_algorithm"),
        checksum_value: row.get("checksum_value"),
        replication_status: row.try_get("replication_status").unwrap_or(None),
        is_tombstone: row.try_get("is_tombstone").unwrap_or(false),
        lock_updated_at: row.try_get("lock_updated_at").unwrap_or(None),
        content_updated_at: row.try_get("content_updated_at").unwrap_or(None),
    }
}

/// Converts a PostgreSQL row to a `BucketInfo`.
fn row_to_bucket_info(row: &sqlx_postgres::PgRow) -> BucketInfo {
    BucketInfo {
        name: row.get("name"),
        created_at: row.get("created_at"),
        owner: row.get("owner"),
    }
}

/// Converts a PostgreSQL row to a `MultipartUploadRecord`.
// `pub(super)`: the control-snapshot builder reads these tables too (R5/D4).
pub(super) fn row_to_multipart_upload_record(row: &sqlx_postgres::PgRow) -> MultipartUploadRecord {
    let metadata_json: serde_json::Value = row.get("metadata");
    let metadata: HashMap<String, String> =
        serde_json::from_value(metadata_json).unwrap_or_default();

    MultipartUploadRecord {
        upload_id: row.get("upload_id"),
        bucket: row.get("bucket"),
        key: row.get("key"),
        content_type: row.get("content_type"),
        initiated_at: row.get("initiated_at"),
        metadata,
        checksum_algorithm: row.get("checksum_algorithm"),
    }
}

/// Converts a PostgreSQL row to a `PartRecord`.
// `pub(super)`: the control-snapshot builder reads these tables too (R5/D4).
pub(super) fn row_to_part_record(row: &sqlx_postgres::PgRow) -> PartRecord {
    PartRecord {
        upload_id: row.get("upload_id"),
        part_number: row.get::<i32, _>("part_number") as u32,
        blob_id: BlobId(row.get("blob_id")),
        size: row.get::<i64, _>("size") as u64,
        etag: row.get("etag"),
        checksum_value: row.get("checksum_value"),
        last_modified: row.get("last_modified"),
    }
}

#[async_trait::async_trait]
impl MetadataStore for PgStore {
    async fn get_stats(&self) -> Result<StorageStats, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT \
                (SELECT COUNT(*) FROM buckets), \
                (SELECT COUNT(*) FROM objects WHERE is_latest = TRUE AND is_delete_marker = FALSE), \
                (SELECT COALESCE(SUM(size), 0) FROM objects WHERE is_latest = TRUE AND is_delete_marker = FALSE)",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_stats: {e}")))?;

        Ok(StorageStats {
            bucket_count: row.get::<i64, _>(0) as u64,
            object_count: row.get::<i64, _>(1) as u64,
            total_size_bytes: row.get::<i64, _>(2) as u64,
        })
    }

    // -- Bucket operations --

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, ArcaError> {
        let rows = sqlx_core::query::query("SELECT name, created_at, owner FROM buckets ORDER BY name")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_buckets: {e}")))?;

        Ok(rows.iter().map(row_to_bucket_info).collect())
    }

    async fn create_bucket(&self, name: &str) -> Result<(), ArcaError> {
        let name_for_err = name.to_string();
        sqlx_core::query::query("INSERT INTO buckets (name, created_at) VALUES ($1, $2)")
            .bind(name)
            .bind(Utc::now())
            .execute(&self.pool)
            .await
            .map_err(|e| {
                let msg = e.to_string();
                if msg.contains("duplicate key") {
                    ArcaError::S3(arca_core::S3Error::new(
                        arca_core::S3ErrorCode::BucketAlreadyOwnedByYou,
                        format!("/{name_for_err}"),
                    ))
                } else {
                    ArcaError::Internal(format!("create_bucket: {e}"))
                }
            })?;

        Ok(())
    }

    async fn head_bucket(&self, name: &str) -> Result<Option<BucketInfo>, ArcaError> {
        let row = sqlx_core::query::query("SELECT name, created_at, owner FROM buckets WHERE name = $1")
            .bind(name)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("head_bucket: {e}")))?;

        Ok(row.as_ref().map(row_to_bucket_info))
    }

    async fn delete_bucket(&self, name: &str) -> Result<bool, ArcaError> {
        // Clean up tags associated with this bucket, then delete the bucket.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_bucket: {e}")))?;

        sqlx_core::query::query("DELETE FROM bucket_tags WHERE bucket = $1")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_bucket: {e}")))?;

        sqlx_core::query::query("DELETE FROM object_tags WHERE bucket = $1")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_bucket: {e}")))?;

        let result = sqlx_core::query::query("DELETE FROM buckets WHERE name = $1")
            .bind(name)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_bucket: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_bucket: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn bucket_is_empty(&self, name: &str) -> Result<bool, ArcaError> {
        let row = sqlx_core::query::query("SELECT COUNT(*) FROM objects WHERE bucket = $1 LIMIT 1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("bucket_is_empty: {e}")))?;

        let count: i64 = row.get(0);
        Ok(count == 0)
    }

    // -- Object operations --

    async fn put_object_if(
        &self,
        record: &ObjectRecord,
        pre: &WritePrecondition,
    ) -> Result<(Option<ObjectRecord>, Option<String>), ArcaError> {
        let mut record = record.clone();
        let resource = format!("/{}/{}", record.bucket, record.key);
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

        // Authoritative CAS check, evaluated inside the same transaction that
        // performs the write, against what `get_object` would see (delete
        // markers filtered).
        if !pre.is_empty() {
            let current = fetch_current_object(&mut tx, &record.bucket, &record.key)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;
            if let Err(code) = pre.evaluate(current.as_ref()) {
                return Err(ArcaError::S3(S3Error::new(code, &resource)));
            }
        }

        let versioning = get_versioning_state(&mut tx, &record.bucket).await;

        let metadata_json = serde_json::to_value(&record.metadata)
            .unwrap_or_else(|_| serde_json::json!({}));

        // Commit-ordered seq for the row this put inserts, taken BEFORE the
        // first row-mutating statement (lock-order rule of next_object_seq).
        let seq = next_object_seq(&mut tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

        let old = match versioning {
            VersioningState::Unversioned => {
                // Find old, DELETE + INSERT.
                let old = fetch_latest_object(&mut tx, &record.bucket, &record.key).await
                    .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                sqlx_core::query::query("DELETE FROM objects WHERE bucket = $1 AND key = $2")
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                // Clean up tags from overwritten object.
                sqlx_core::query::query("DELETE FROM object_tags WHERE bucket = $1 AND key = $2")
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                record.version_id = None;
                record.is_latest = true;
                record.is_delete_marker = false;
                insert_object_row(&mut tx, &record, &metadata_json, seq).await
                    .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                old // old blob to clean up
            }
            VersioningState::Enabled => {
                // Mark current latest as not-latest.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = FALSE \
                     WHERE bucket = $1 AND key = $2 AND is_latest = TRUE",
                )
                .bind(&record.bucket)
                .bind(&record.key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                // Generate version ID and insert new row.
                record.version_id = Some(uuid::Uuid::new_v4().to_string());
                record.is_latest = true;
                record.is_delete_marker = false;
                insert_object_row(&mut tx, &record, &metadata_json, seq).await
                    .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                None // keep old versions, no cleanup
            }
            VersioningState::Suspended => {
                // Delete existing null-version (if any) for cleanup.
                let old_null = fetch_null_version(&mut tx, &record.bucket, &record.key).await
                    .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                )
                .bind(&record.bucket)
                .bind(&record.key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                // Clean up tags from old null-version.
                sqlx_core::query::query(
                    "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = ''",
                )
                .bind(&record.bucket)
                .bind(&record.key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                // Mark any remaining latest as not-latest.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = FALSE \
                     WHERE bucket = $1 AND key = $2 AND is_latest = TRUE",
                )
                .bind(&record.bucket)
                .bind(&record.key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                // Insert with NULL version_id.
                record.version_id = None;
                record.is_latest = true;
                record.is_delete_marker = false;
                insert_object_row(&mut tx, &record, &metadata_json, seq).await
                    .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

                old_null // clean up old null-version blob
            }
        };

        // Finalize is_latest deterministically so the origin and cluster replicas
        // (which run recompute in apply_remote_object) always agree on the current
        // version (Phase 29, Risk #1). Matches the SQLite backend.
        recompute_is_latest(&mut tx, &record.bucket, &record.key)
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object_if: {e}")))?;

        Ok((old, record.version_id.clone()))
    }

    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let sql = format!(
            "SELECT {OBJECT_COLUMNS} FROM objects \
             WHERE bucket = $1 AND key = $2 AND is_latest = TRUE AND is_delete_marker = FALSE"
        );
        let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .bind(bucket)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("get_object: {e}")))?;

        Ok(row.as_ref().map(row_to_object_record))
    }

    async fn get_latest_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let sql = format!(
            "SELECT {OBJECT_COLUMNS} FROM objects \
             WHERE bucket = $1 AND key = $2 AND is_latest = TRUE"
        );
        let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .bind(bucket)
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("get_latest_object: {e}")))?;

        Ok(row.as_ref().map(row_to_object_record))
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        // Build dynamic SQL with positional parameters.
        let mut sql = format!(
            "SELECT {OBJECT_COLUMNS} FROM objects \
             WHERE bucket = $1 AND is_latest = TRUE AND is_delete_marker = FALSE"
        );
        let mut param_idx = 2u32;

        let prefix_pattern = prefix.map(|p| {
            let idx = param_idx;
            param_idx += 1;
            sql.push_str(&format!(" AND key LIKE ${idx} ESCAPE '\\'"));
            format!("{}%", escape_like(p))
        });

        if start_after.is_some() {
            let idx = param_idx;
            #[allow(unused_assignments)]
            {
                param_idx += 1;
            }
            sql.push_str(&format!(" AND key > ${idx}"));
        }

        sql.push_str(" ORDER BY key");
        sql.push_str(&format!(" LIMIT {max_keys}"));

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str())).bind(bucket);
        if let Some(ref pattern) = prefix_pattern {
            query = query.bind(pattern.clone());
        }
        if let Some(sa) = start_after {
            query = query.bind(sa);
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_objects: {e}")))?;

        Ok(rows.iter().map(row_to_object_record).collect())
    }

    async fn delete_object_if(
        &self,
        bucket: &str,
        key: &str,
        pre: &DeletePrecondition,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let cluster_mode = self.cluster_mode();
        let resource = format!("/{bucket}/{key}");
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

        // Authoritative CAS check against the object being deleted, as
        // `MetadataStore::get_latest_object` would see it (includes a delete
        // marker). An absent object is always `Ok(())` — DeleteObject on a
        // missing key is a no-op, never a precondition failure (existing
        // behaviour, plan §3.5).
        if !pre.is_empty() {
            let current = fetch_latest_object(&mut tx, bucket, key)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;
            if let Err(code) = pre.evaluate(current.as_ref()) {
                return Err(ArcaError::S3(S3Error::new(code, &resource)));
            }
        }

        let versioning = get_versioning_state(&mut tx, bucket).await;

        let result = match versioning {
            VersioningState::Unversioned => {
                let old = fetch_latest_object(&mut tx, bucket, key)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                if old.is_some() {
                    // Clustered: tombstone (blob cleared) so the deletion
                    // converges via the manifest and isn't resurrected by
                    // anti-entropy. Single-node: remove the row.
                    if cluster_mode {
                        // Commit-ordered seq, taken before the first DML
                        // (lock-order rule of next_object_seq). One seq for
                        // the single row of this key (an unversioned bucket
                        // was never versioned -> at most one row matches).
                        let seq = next_object_seq(&mut tx)
                            .await
                            .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;
                        sqlx_core::query::query(
                            "UPDATE objects SET seq = $4, is_tombstone = TRUE, is_delete_marker = FALSE, \
                             blob_id = '', size = 0, last_modified = $3 \
                             WHERE bucket = $1 AND key = $2",
                        )
                        .bind(bucket)
                        .bind(key)
                        .bind(Utc::now())
                        .bind(seq)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;
                    } else {
                        sqlx_core::query::query("DELETE FROM objects WHERE bucket = $1 AND key = $2")
                            .bind(bucket)
                            .bind(key)
                            .execute(&mut *tx)
                            .await
                            .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;
                    }

                    // Clean up tags either way.
                    sqlx_core::query::query("DELETE FROM object_tags WHERE bucket = $1 AND key = $2")
                        .bind(bucket)
                        .bind(key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                    if cluster_mode {
                        recompute_is_latest(&mut tx, bucket, key)
                            .await
                            .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;
                    }
                }

                old // blob to clean up
            }
            VersioningState::Enabled => {
                // Commit-ordered seq for the delete marker, taken before the
                // first DML (lock-order rule of next_object_seq).
                let seq = next_object_seq(&mut tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                // Mark current latest as not-latest.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = FALSE \
                     WHERE bucket = $1 AND key = $2 AND is_latest = TRUE",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                // Insert a delete marker.
                let version_id = uuid::Uuid::new_v4().to_string();
                let now = Utc::now();
                let empty_metadata = serde_json::json!({});
                sqlx_core::query::query(
                    "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, \
                     last_modified, metadata, encryption_algorithm, encryption_key_id, owner, \
                     version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, \
                     legal_hold_status, storage_class, checksum_algorithm, checksum_value, seq) \
                     VALUES ($1, $2, '', 0, '', NULL, $3, $4, NULL, NULL, 'root', $5, TRUE, TRUE, \
                     NULL, NULL, NULL, 'STANDARD', NULL, NULL, $6)",
                )
                .bind(bucket)
                .bind(key)
                .bind(now)
                .bind(&empty_metadata)
                .bind(&version_id)
                .bind(seq)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                // Return the delete marker so the handler can set response headers.
                Some(ObjectRecord {
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    blob_id: BlobId(String::new()),
                    size: 0,
                    etag: String::new(),
                    content_type: None,
                    last_modified: now,
                    metadata: HashMap::new(),
                    encryption_algorithm: None,
                    encryption_key_id: None,
                    owner: "root".to_string(),
                    version_id: Some(version_id),
                    is_latest: true,
                    is_delete_marker: true,
                    is_tombstone: false,
                    retention_mode: None,
                    retain_until_date: None,
                    legal_hold_status: None,
                    storage_class: "STANDARD".to_string(),
                    checksum_algorithm: None,
                    checksum_value: None,
                    replication_status: None,
                    lock_updated_at: None,
                    content_updated_at: None,
                })
            }
            VersioningState::Suspended => {
                // Delete existing null-version for cleanup.
                let old_null = fetch_null_version(&mut tx, bucket, key)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                // Commit-ordered seq for the delete marker, taken before the
                // first DML (lock-order rule of next_object_seq).
                let seq = next_object_seq(&mut tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                // Clean up tags from old null-version.
                sqlx_core::query::query(
                    "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = ''",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                // Mark any remaining latest as not-latest.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = FALSE \
                     WHERE bucket = $1 AND key = $2 AND is_latest = TRUE",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                // Insert delete marker with NULL version_id.
                let now = Utc::now();
                let empty_metadata = serde_json::json!({});
                sqlx_core::query::query(
                    "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, \
                     last_modified, metadata, encryption_algorithm, encryption_key_id, owner, \
                     version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, \
                     legal_hold_status, storage_class, checksum_algorithm, checksum_value, seq) \
                     VALUES ($1, $2, '', 0, '', NULL, $3, $4, NULL, NULL, 'root', NULL, TRUE, TRUE, \
                     NULL, NULL, NULL, 'STANDARD', NULL, NULL, $5)",
                )
                .bind(bucket)
                .bind(key)
                .bind(now)
                .bind(&empty_metadata)
                .bind(seq)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

                old_null // clean up old null-version blob (if any)
            }
        };

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object_if: {e}")))?;

        Ok(result)
    }

    // -- Versioned object operations --

    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let row = if version_id == "null" {
            let sql = format!(
                "SELECT {OBJECT_COLUMNS} FROM objects \
                 WHERE bucket = $1 AND key = $2 AND version_id IS NULL AND is_tombstone = FALSE"
            );
            sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
                .bind(bucket)
                .bind(key)
                .fetch_optional(&self.pool)
                .await
        } else {
            let sql = format!(
                "SELECT {OBJECT_COLUMNS} FROM objects \
                 WHERE bucket = $1 AND key = $2 AND version_id = $3 AND is_tombstone = FALSE"
            );
            sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
                .bind(bucket)
                .bind(key)
                .bind(version_id)
                .fetch_optional(&self.pool)
                .await
        };

        let row = row.map_err(|e| ArcaError::Internal(format!("get_object_version: {e}")))?;
        Ok(row.as_ref().map(row_to_object_record))
    }

    async fn delete_object_version_if(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        pre: &DeletePrecondition,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let cluster_mode = self.cluster_mode();
        let resource = format!("/{bucket}/{key}?versionId={version_id}");
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;

        // Fetch the version to delete.
        let deleted = if version_id == "null" {
            let sql = format!(
                "SELECT {OBJECT_COLUMNS} FROM objects \
                 WHERE bucket = $1 AND key = $2 AND version_id IS NULL AND is_tombstone = FALSE"
            );
            let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
                .bind(bucket)
                .bind(key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
            row.as_ref().map(row_to_object_record)
        } else {
            let sql = format!(
                "SELECT {OBJECT_COLUMNS} FROM objects \
                 WHERE bucket = $1 AND key = $2 AND version_id = $3 AND is_tombstone = FALSE"
            );
            let row = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
                .bind(bucket)
                .bind(key)
                .bind(version_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
            row.as_ref().map(row_to_object_record)
        };

        // Per S3 semantics (and the existing single-version delete handler),
        // the precondition is skipped entirely — never evaluated, never
        // refused — when the targeted version is itself a delete marker.
        if !pre.is_empty() {
            let applies = match &deleted {
                Some(rec) => !rec.is_delete_marker,
                None => true,
            };
            if applies {
                if let Err(code) = pre.evaluate(deleted.as_ref()) {
                    return Err(ArcaError::S3(S3Error::new(code, &resource)));
                }
            }
        }

        if deleted.is_some() {
            // Clustered: tombstone the version (blob cleared) so the deletion
            // converges via the manifest and isn't resurrected by anti-entropy.
            // Single-node: remove the row.
            if cluster_mode {
                // Commit-ordered seq, taken before the first DML (lock-order
                // rule of next_object_seq). The WHERE targets one version row.
                let seq = next_object_seq(&mut tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
                let now = Utc::now();
                if version_id == "null" {
                    sqlx_core::query::query(
                        "UPDATE objects SET seq = $4, is_tombstone = TRUE, is_delete_marker = FALSE, \
                         blob_id = '', size = 0, last_modified = $3 \
                         WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                    )
                    .bind(bucket)
                    .bind(key)
                    .bind(now)
                    .bind(seq)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
                } else {
                    sqlx_core::query::query(
                        "UPDATE objects SET seq = $5, is_tombstone = TRUE, is_delete_marker = FALSE, \
                         blob_id = '', size = 0, last_modified = $4 \
                         WHERE bucket = $1 AND key = $2 AND version_id = $3",
                    )
                    .bind(bucket)
                    .bind(key)
                    .bind(version_id)
                    .bind(now)
                    .bind(seq)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
                }
            } else if version_id == "null" {
                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
            } else {
                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id = $3",
                )
                .bind(bucket)
                .bind(key)
                .bind(version_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
            }

            // Clean up tags for this version.
            let tag_vid = if version_id == "null" {
                String::new()
            } else {
                version_id.to_string()
            };
            sqlx_core::query::query(
                "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = $3",
            )
            .bind(bucket)
            .bind(key)
            .bind(&tag_vid)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;

            // Recompute is_latest deterministically (full tiebreak: last_modified
            // DESC, version_id DESC NULLS LAST, blob_id DESC), matching SQLite and
            // apply_remote_* so every node agrees on the current version. Replaces
            // the previous ORDER BY last_modified DESC promotion, which lacked the
            // tiebreak and could diverge from peers (Phase 29, Risk #1).
            recompute_is_latest(&mut tx, bucket, key)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object_version_if: {e}")))?;

        Ok(deleted)
    }

    async fn apply_remote_object(&self, record: &ObjectRecord) -> Result<(), ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?;

        // Fetch the local row for this exact version (null-version rows form a
        // single LWW register per (bucket, key); versioned rows are keyed by
        // version_id). The write decision is centralized in
        // ObjectRecord::resolve_replicated: strictly newer last_modified
        // replaces the whole row; on a tie the lock register (lock_updated_at)
        // and the content register (content_updated_at) are merged
        // independently so a re-encryption and a lock change never clobber each
        // other (N2 ordering + Phase 30 WORM safety, TD-021), and an identical
        // redelivery is skipped without a rewrite so two caught-up nodes don't
        // redeliver forever (M7).
        let existing: Option<ObjectRecord> = match &record.version_id {
            Some(vid) => {
                let sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects \
                     WHERE bucket = $1 AND key = $2 AND version_id = $3"
                );
                sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .bind(vid)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?
                    .map(|row| row_to_object_record(&row))
            }
            None => {
                let sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects \
                     WHERE bucket = $1 AND key = $2 AND version_id IS NULL"
                );
                sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?
                    .map(|row| row_to_object_record(&row))
            }
        };

        let to_write = match &existing {
            None => Some(record.clone()),
            Some(ex) => record.resolve_replicated(ex),
        };

        if let Some(row) = to_write {
            let metadata_json =
                serde_json::to_value(&row.metadata).unwrap_or_else(|_| serde_json::json!({}));
            // Commit-ordered seq before the first DML (lock-order rule of
            // next_object_seq). Re-stamped on every effective apply so the
            // reconciliation propagates transitively A->B->C.
            let seq = next_object_seq(&mut tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?;
            match &record.version_id {
                Some(vid) => {
                    sqlx_core::query::query(
                        "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id = $3",
                    )
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .bind(vid)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?;
                }
                None => {
                    sqlx_core::query::query(
                        "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                    )
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?;
                }
            }
            insert_replicated_row(&mut tx, &row, &metadata_json, seq)
                .await
                .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?;
        }

        recompute_is_latest(&mut tx, &record.bucket, &record.key)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_object: {e}")))?;
        Ok(())
    }

    async fn apply_remote_version_delete(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;

        // Tombstone (don't remove) so this node's manifest carries the deletion
        // onward and anti-entropy can't resurrect it. The `is_tombstone = FALSE`
        // guard makes re-delivery a no-op (no re-propagation churn). Absent row
        // → no-op; the origin's tombstone still arrives via anti-entropy.
        //
        // Commit-ordered seq before the first DML (lock-order rule of
        // next_object_seq); a re-delivery no-op burns the value (harmless gap).
        let seq = next_object_seq(&mut tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;
        let now = Utc::now();
        if version_id == "null" {
            sqlx_core::query::query(
                "UPDATE objects SET seq = $4, is_tombstone = TRUE, is_delete_marker = FALSE, blob_id = '', \
                 size = 0, last_modified = $3 \
                 WHERE bucket = $1 AND key = $2 AND version_id IS NULL AND is_tombstone = FALSE",
            )
            .bind(bucket)
            .bind(key)
            .bind(now)
            .bind(seq)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;
            sqlx_core::query::query(
                "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = ''",
            )
            .bind(bucket)
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;
        } else {
            sqlx_core::query::query(
                "UPDATE objects SET seq = $5, is_tombstone = TRUE, is_delete_marker = FALSE, blob_id = '', \
                 size = 0, last_modified = $4 \
                 WHERE bucket = $1 AND key = $2 AND version_id = $3 AND is_tombstone = FALSE",
            )
            .bind(bucket)
            .bind(key)
            .bind(version_id)
            .bind(now)
            .bind(seq)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;
            sqlx_core::query::query(
                "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = $3",
            )
            .bind(bucket)
            .bind(key)
            .bind(version_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;
        }

        recompute_is_latest(&mut tx, bucket, key)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_remote_version_delete: {e}")))?;
        Ok(())
    }

    async fn apply_remote_bucket(&self, info: &BucketInfo) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO buckets (name, created_at, owner) VALUES ($1, $2, $3) \
             ON CONFLICT (name) DO UPDATE SET created_at = EXCLUDED.created_at, owner = EXCLUDED.owner",
        )
        .bind(&info.name)
        .bind(info.created_at)
        .bind(&info.owner)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_remote_bucket: {e}")))?;
        Ok(())
    }

    async fn apply_remote_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        let metadata_json =
            serde_json::to_value(&record.metadata).unwrap_or_else(|_| serde_json::json!({}));
        // Immutable once created (upload_id is the key): re-delivery is a no-op.
        sqlx_core::query::query(
            "INSERT INTO multipart_uploads \
             (upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (upload_id) DO NOTHING",
        )
        .bind(&record.upload_id)
        .bind(&record.bucket)
        .bind(&record.key)
        .bind(&record.content_type)
        .bind(record.initiated_at)
        .bind(&metadata_json)
        .bind(&record.checksum_algorithm)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_remote_multipart_upload: {e}")))?;
        Ok(())
    }

    async fn list_rows_changed_since(
        &self,
        since: u64,
        limit: u32,
    ) -> Result<Vec<(u64, ObjectRecord)>, ArcaError> {
        let sql = format!(
            "SELECT {OBJECT_COLUMNS}, seq FROM objects \
             WHERE seq > $1 ORDER BY seq ASC LIMIT $2"
        );
        let rows = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .bind(since as i64)
            .bind(limit as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_rows_changed_since: {e}")))?;
        Ok(rows
            .iter()
            .map(|row| (row.get::<i64, _>("seq") as u64, row_to_object_record(row)))
            .collect())
    }

    async fn purge_tombstones(
        &self,
        before: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, ArcaError> {
        let result = sqlx_core::query::query(
            "DELETE FROM objects WHERE is_tombstone = TRUE AND last_modified < $1",
        )
        .bind(before)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("purge_tombstones: {e}")))?;
        Ok(result.rows_affected())
    }

    async fn current_object_seq(&self) -> Result<u64, ArcaError> {
        // The counter, not MAX(seq) over rows: purged tombstones make the row
        // maximum go backwards, which would false-alarm D3c rewind detection.
        let row = sqlx_core::query::query("SELECT value FROM object_seq")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("current_object_seq: {e}")))?;
        Ok(row.get::<i64, _>("value") as u64)
    }

    async fn seed_object_seq_to_max(&self) -> Result<u64, ArcaError> {
        // Bump the counter UP to MAX(seq) when it lags; never rewind it (a
        // rewind would trip peer D3c rewind detection). A single statement
        // does the conditional update atomically.
        let row = sqlx_core::query::query(
            "UPDATE object_seq \
             SET value = GREATEST(value, (SELECT COALESCE(MAX(seq), 0) FROM objects)) \
             RETURNING value",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("seed_object_seq_to_max: {e}")))?;
        Ok(row.get::<i64, _>("value") as u64)
    }

    async fn list_referenced_blob_ids(&self) -> Result<Vec<BlobId>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT blob_id FROM objects WHERE is_tombstone = FALSE AND blob_id != ''
             UNION
             SELECT blob_id FROM parts",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_referenced_blob_ids: {e}")))?;
        Ok(rows
            .iter()
            .map(|r| BlobId(r.get::<String, _>("blob_id")))
            .collect())
    }

    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        _version_id_marker: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        let mut sql = format!(
            "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = $1 AND is_tombstone = FALSE"
        );
        let mut param_idx = 2u32;

        let prefix_pattern = prefix.map(|p| {
            let idx = param_idx;
            param_idx += 1;
            sql.push_str(&format!(" AND key LIKE ${idx} ESCAPE '\\'"));
            format!("{}%", escape_like(p))
        });

        let has_key_marker = key_marker.map_or(false, |km| !km.is_empty());
        if has_key_marker {
            let idx = param_idx;
            #[allow(unused_assignments)]
            {
                param_idx += 1;
            }
            sql.push_str(&format!(" AND key > ${idx}"));
        }

        sql.push_str(" ORDER BY key ASC, last_modified DESC");
        sql.push_str(&format!(" LIMIT {max_keys}"));

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str())).bind(bucket);
        if let Some(ref pattern) = prefix_pattern {
            query = query.bind(pattern.clone());
        }
        if let Some(km) = key_marker {
            if !km.is_empty() {
                query = query.bind(km);
            }
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_object_versions: {e}")))?;

        Ok(rows.iter().map(row_to_object_record).collect())
    }

    // -- Bucket config operations --

    async fn get_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<Option<String>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT config_value FROM bucket_config WHERE bucket = $1 AND config_key = $2",
        )
        .bind(bucket)
        .bind(config_key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_bucket_config: {e}")))?;

        Ok(row.map(|r| r.get("config_value")))
    }

    async fn set_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO bucket_config (bucket, config_key, config_value, updated_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (bucket, config_key) DO UPDATE SET config_value = $3, updated_at = $4",
        )
        .bind(bucket)
        .bind(config_key)
        .bind(config_value)
        .bind(Utc::now())
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("set_bucket_config: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<bool, ArcaError> {
        let result =
            sqlx_core::query::query("DELETE FROM bucket_config WHERE bucket = $1 AND config_key = $2")
                .bind(bucket)
                .bind(config_key)
                .execute(&self.pool)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_bucket_config: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    // -- Tag operations --

    async fn get_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<Vec<(String, String)>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT tag_key, tag_value FROM bucket_tags WHERE bucket = $1 ORDER BY tag_key",
        )
        .bind(bucket)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_bucket_tags: {e}")))?;

        Ok(rows.iter().map(|r| (r.get("tag_key"), r.get("tag_value"))).collect())
    }

    async fn put_bucket_tags(
        &self,
        bucket: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_bucket_tags: {e}")))?;

        sqlx_core::query::query("DELETE FROM bucket_tags WHERE bucket = $1")
            .bind(bucket)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("put_bucket_tags: {e}")))?;

        // One timestamp for the whole set: the control reconcile (R5) treats a
        // bucket's tags as a single LWW entity (replace-all semantics).
        let now = Utc::now();
        for (k, v) in tags {
            sqlx_core::query::query(
                "INSERT INTO bucket_tags (bucket, tag_key, tag_value, updated_at) VALUES ($1, $2, $3, $4)",
            )
            .bind(bucket)
            .bind(k)
            .bind(v)
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("put_bucket_tags: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_bucket_tags: {e}")))?;

        Ok(())
    }

    async fn delete_bucket_tags(&self, bucket: &str) -> Result<bool, ArcaError> {
        let result = sqlx_core::query::query("DELETE FROM bucket_tags WHERE bucket = $1")
            .bind(bucket)
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_bucket_tags: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn apply_bucket_config_at(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        // Like set_bucket_config, but preserving the source's updated_at (the
        // R5 reconcile LWW key) instead of stamping now().
        sqlx_core::query::query(
            "INSERT INTO bucket_config (bucket, config_key, config_value, updated_at)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (bucket, config_key) DO UPDATE SET
               config_value = EXCLUDED.config_value,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(bucket)
        .bind(config_key)
        .bind(config_value)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_bucket_config_at: {e}")))?;
        Ok(())
    }

    async fn apply_bucket_tags_at(
        &self,
        bucket: &str,
        tags: &[(String, String)],
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        // Like put_bucket_tags, but preserving the source's updated_at.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_bucket_tags_at: {e}")))?;
        sqlx_core::query::query("DELETE FROM bucket_tags WHERE bucket = $1")
            .bind(bucket)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_bucket_tags_at: {e}")))?;
        for (k, v) in tags {
            sqlx_core::query::query(
                "INSERT INTO bucket_tags (bucket, tag_key, tag_value, updated_at) VALUES ($1, $2, $3, $4)",
            )
            .bind(bucket)
            .bind(k)
            .bind(v)
            .bind(updated_at)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_bucket_tags_at: {e}")))?;
        }
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("apply_bucket_tags_at: {e}")))?;
        Ok(())
    }

    async fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Vec<(String, String)>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT tag_key, tag_value FROM object_tags \
             WHERE bucket = $1 AND key = $2 AND version_id = $3 \
             ORDER BY tag_key",
        )
        .bind(bucket)
        .bind(key)
        .bind(version_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_object_tags: {e}")))?;

        Ok(rows.iter().map(|r| (r.get("tag_key"), r.get("tag_value"))).collect())
    }

    async fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object_tags: {e}")))?;

        sqlx_core::query::query(
            "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = $3",
        )
        .bind(bucket)
        .bind(key)
        .bind(version_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("put_object_tags: {e}")))?;

        for (k, v) in tags {
            sqlx_core::query::query(
                "INSERT INTO object_tags (bucket, key, version_id, tag_key, tag_value) \
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(bucket)
            .bind(key)
            .bind(version_id)
            .bind(k)
            .bind(v)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object_tags: {e}")))?;
        }

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object_tags: {e}")))?;

        Ok(())
    }

    async fn delete_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<bool, ArcaError> {
        let result = sqlx_core::query::query(
            "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = $3",
        )
        .bind(bucket)
        .bind(key)
        .bind(version_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("delete_object_tags: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    // -- Multipart upload operations --

    async fn create_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        let metadata_json = serde_json::to_value(&record.metadata)
            .unwrap_or_else(|_| serde_json::json!({}));

        sqlx_core::query::query(
            "INSERT INTO multipart_uploads \
             (upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&record.upload_id)
        .bind(&record.bucket)
        .bind(&record.key)
        .bind(&record.content_type)
        .bind(record.initiated_at)
        .bind(&metadata_json)
        .bind(&record.checksum_algorithm)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("create_multipart_upload: {e}")))?;

        Ok(())
    }

    async fn get_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Option<MultipartUploadRecord>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm \
             FROM multipart_uploads WHERE upload_id = $1",
        )
        .bind(upload_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_multipart_upload: {e}")))?;

        Ok(row.as_ref().map(row_to_multipart_upload_record))
    }

    async fn put_part(&self, part: &PartRecord) -> Result<Option<PartRecord>, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_part: {e}")))?;

        // Check for existing part to return for cleanup.
        let old = sqlx_core::query::query(
            "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified \
             FROM parts WHERE upload_id = $1 AND part_number = $2",
        )
        .bind(&part.upload_id)
        .bind(part.part_number as i32)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("put_part: {e}")))?;

        let old = old.as_ref().map(row_to_part_record);

        // Delete old part if exists, then insert new.
        sqlx_core::query::query("DELETE FROM parts WHERE upload_id = $1 AND part_number = $2")
            .bind(&part.upload_id)
            .bind(part.part_number as i32)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("put_part: {e}")))?;

        sqlx_core::query::query(
            "INSERT INTO parts (upload_id, part_number, blob_id, size, etag, checksum_value, last_modified) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&part.upload_id)
        .bind(part.part_number as i32)
        .bind(&part.blob_id.0)
        .bind(part.size as i64)
        .bind(&part.etag)
        .bind(&part.checksum_value)
        .bind(part.last_modified)
        .execute(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("put_part: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_part: {e}")))?;

        Ok(old)
    }

    async fn list_parts(&self, upload_id: &str) -> Result<Vec<PartRecord>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified \
             FROM parts WHERE upload_id = $1 ORDER BY part_number",
        )
        .bind(upload_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_parts: {e}")))?;

        Ok(rows.iter().map(row_to_part_record).collect())
    }

    async fn delete_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_multipart_upload: {e}")))?;

        // Collect parts for blob cleanup.
        let rows = sqlx_core::query::query(
            "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified \
             FROM parts WHERE upload_id = $1",
        )
        .bind(upload_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| ArcaError::Internal(format!("delete_multipart_upload: {e}")))?;

        let parts: Vec<PartRecord> = rows.iter().map(row_to_part_record).collect();

        // Delete parts and upload record.
        sqlx_core::query::query("DELETE FROM parts WHERE upload_id = $1")
            .bind(upload_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_multipart_upload: {e}")))?;

        sqlx_core::query::query("DELETE FROM multipart_uploads WHERE upload_id = $1")
            .bind(upload_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_multipart_upload: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_multipart_upload: {e}")))?;

        Ok(parts)
    }

    async fn list_multipart_uploads(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        upload_id_marker: Option<&str>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        let mut sql = String::from(
            "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm \
             FROM multipart_uploads WHERE bucket = $1",
        );
        let mut param_idx = 2u32;

        let prefix_pattern = prefix.map(|p| {
            let idx = param_idx;
            param_idx += 1;
            sql.push_str(&format!(" AND key LIKE ${idx} ESCAPE '\\'"));
            format!("{}%", escape_like(p))
        });

        // Pagination: key_marker + upload_id_marker
        // Track which extra bindings we need.
        #[derive(Default)]
        struct PaginationBinds {
            key_marker_only: bool,
            key_and_upload: bool,
        }
        let mut pagination = PaginationBinds::default();

        if let Some(km) = key_marker {
            if let Some(uim) = upload_id_marker {
                if !uim.is_empty() {
                    let kid = param_idx;
                    param_idx += 1;
                    let uid = param_idx;
                    #[allow(unused_assignments)]
                    {
                        param_idx += 1;
                    }
                    sql.push_str(&format!(
                        " AND (key > ${kid} OR (key = ${kid} AND upload_id > ${uid}))"
                    ));
                    pagination.key_and_upload = true;
                } else if !km.is_empty() {
                    let idx = param_idx;
                    #[allow(unused_assignments)]
                    {
                        param_idx += 1;
                    }
                    sql.push_str(&format!(" AND key > ${idx}"));
                    pagination.key_marker_only = true;
                }
            } else if !km.is_empty() {
                let idx = param_idx;
                #[allow(unused_assignments)]
                {
                    param_idx += 1;
                }
                sql.push_str(&format!(" AND key > ${idx}"));
                pagination.key_marker_only = true;
            }
        }

        sql.push_str(" ORDER BY key, upload_id");
        sql.push_str(&format!(" LIMIT {max_uploads}"));

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str())).bind(bucket);
        if let Some(ref pattern) = prefix_pattern {
            query = query.bind(pattern.clone());
        }
        if pagination.key_and_upload {
            query = query.bind(key_marker.unwrap());
            query = query.bind(upload_id_marker.unwrap());
        } else if pagination.key_marker_only {
            query = query.bind(key_marker.unwrap());
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_multipart_uploads: {e}")))?;

        Ok(rows.iter().map(row_to_multipart_upload_record).collect())
    }

    // -- Object Lock operations --

    async fn set_object_retention(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        retention_mode: Option<&str>,
        retain_until_date: Option<&str>,
    ) -> Result<bool, ArcaError> {
        // Parse the retain_until_date string to DateTime<Utc> for TIMESTAMPTZ binding.
        let retain_dt: Option<DateTime<Utc>> = retain_until_date
            .map(|s| {
                DateTime::parse_from_rfc3339(s)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        ArcaError::Internal(format!("set_object_retention: invalid date: {e}"))
                    })
            })
            .transpose()?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("set_object_retention: {e}")))?;
        // Fresh seq so the lock change travels via the changed-since manifest
        // to peers that miss the real-time fan-out (N1). Taken before the row
        // UPDATE per the next_object_seq lock-order rule. lock_updated_at
        // orders the lock state across nodes (N2): last_modified does not
        // change here, so without it a stale equal-timestamp copy applied
        // later would clobber this.
        let seq = next_object_seq(&mut tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("set_object_retention: {e}")))?;
        let lock_updated_at = Utc::now();
        let result = if let Some(vid) = version_id {
            sqlx_core::query::query(
                "UPDATE objects SET retention_mode = $1, retain_until_date = $2, seq = $3, lock_updated_at = $4 \
                 WHERE bucket = $5 AND key = $6 AND version_id = $7",
            )
            .bind(retention_mode)
            .bind(retain_dt)
            .bind(seq)
            .bind(lock_updated_at)
            .bind(bucket)
            .bind(key)
            .bind(vid)
            .execute(&mut *tx)
            .await
        } else {
            sqlx_core::query::query(
                "UPDATE objects SET retention_mode = $1, retain_until_date = $2, seq = $3, lock_updated_at = $4 \
                 WHERE bucket = $5 AND key = $6 AND is_latest = TRUE",
            )
            .bind(retention_mode)
            .bind(retain_dt)
            .bind(seq)
            .bind(lock_updated_at)
            .bind(bucket)
            .bind(key)
            .execute(&mut *tx)
            .await
        };

        let result =
            result.map_err(|e| ArcaError::Internal(format!("set_object_retention: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("set_object_retention: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_object_legal_hold(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("set_object_legal_hold: {e}")))?;
        // Fresh seq for manifest visibility (N1) and lock_updated_at for the
        // lock-state LWW (N2) — see set_object_retention.
        let seq = next_object_seq(&mut tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("set_object_legal_hold: {e}")))?;
        let lock_updated_at = Utc::now();
        let result = if let Some(vid) = version_id {
            sqlx_core::query::query(
                "UPDATE objects SET legal_hold_status = $1, seq = $2, lock_updated_at = $3 \
                 WHERE bucket = $4 AND key = $5 AND version_id = $6",
            )
            .bind(status)
            .bind(seq)
            .bind(lock_updated_at)
            .bind(bucket)
            .bind(key)
            .bind(vid)
            .execute(&mut *tx)
            .await
        } else {
            sqlx_core::query::query(
                "UPDATE objects SET legal_hold_status = $1, seq = $2, lock_updated_at = $3 \
                 WHERE bucket = $4 AND key = $5 AND is_latest = TRUE",
            )
            .bind(status)
            .bind(seq)
            .bind(lock_updated_at)
            .bind(bucket)
            .bind(key)
            .execute(&mut *tx)
            .await
        };

        let result =
            result.map_err(|e| ArcaError::Internal(format!("set_object_legal_hold: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("set_object_legal_hold: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_object_encryption(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        algorithm: Option<&str>,
        key_id: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("update_object_encryption: {e}")))?;
        // Fresh seq so the re-encryption reaches peers via the changed-since
        // manifest; last_modified/ETag untouched (the logical object is
        // unchanged). A fresh content_updated_at is the cluster LWW convergence
        // dimension for the CONTENT column group (blob/algorithm/key), kept
        // separate from the lock register's lock_updated_at so a re-encryption
        // and a concurrent lock change merge independently instead of clobbering
        // each other (TD-021 — see apply_remote_object). Seq before the row
        // UPDATE per the lock-order rule.
        let seq = next_object_seq(&mut tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("update_object_encryption: {e}")))?;
        let content_updated_at = Utc::now();
        let result = if let Some(vid) = version_id {
            sqlx_core::query::query(
                "UPDATE objects SET encryption_algorithm = $1, encryption_key_id = $2, seq = $3, content_updated_at = $4 \
                 WHERE bucket = $5 AND key = $6 AND version_id = $7",
            )
            .bind(algorithm)
            .bind(key_id)
            .bind(seq)
            .bind(content_updated_at)
            .bind(bucket)
            .bind(key)
            .bind(vid)
            .execute(&mut *tx)
            .await
        } else {
            sqlx_core::query::query(
                "UPDATE objects SET encryption_algorithm = $1, encryption_key_id = $2, seq = $3, content_updated_at = $4 \
                 WHERE bucket = $5 AND key = $6 AND is_latest = TRUE",
            )
            .bind(algorithm)
            .bind(key_id)
            .bind(seq)
            .bind(content_updated_at)
            .bind(bucket)
            .bind(key)
            .execute(&mut *tx)
            .await
        };
        let result =
            result.map_err(|e| ArcaError::Internal(format!("update_object_encryption: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("update_object_encryption: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_object_encryption_cas(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        old_blob_id: &BlobId,
        new_blob_id: &BlobId,
        algorithm: Option<&str>,
        key_id: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("update_object_encryption_cas: {e}")))?;
        let seq = next_object_seq(&mut tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("update_object_encryption_cas: {e}")))?;
        // CAS guard on blob_id: 0 rows means a concurrent client overwrite
        // already swapped the blob and the caller discards the new one. A fresh
        // content_updated_at is the cluster LWW convergence dimension for the
        // content column group (see update_object_encryption, TD-021).
        let content_updated_at = Utc::now();
        let result = if let Some(vid) = version_id {
            sqlx_core::query::query(
                "UPDATE objects SET blob_id = $1, encryption_algorithm = $2, encryption_key_id = $3, seq = $4, content_updated_at = $5 \
                 WHERE bucket = $6 AND key = $7 AND version_id = $8 AND blob_id = $9",
            )
            .bind(&new_blob_id.0)
            .bind(algorithm)
            .bind(key_id)
            .bind(seq)
            .bind(content_updated_at)
            .bind(bucket)
            .bind(key)
            .bind(vid)
            .bind(&old_blob_id.0)
            .execute(&mut *tx)
            .await
        } else {
            sqlx_core::query::query(
                "UPDATE objects SET blob_id = $1, encryption_algorithm = $2, encryption_key_id = $3, seq = $4, content_updated_at = $5 \
                 WHERE bucket = $6 AND key = $7 AND is_latest = TRUE AND blob_id = $8",
            )
            .bind(&new_blob_id.0)
            .bind(algorithm)
            .bind(key_id)
            .bind(seq)
            .bind(content_updated_at)
            .bind(bucket)
            .bind(key)
            .bind(&old_blob_id.0)
            .execute(&mut *tx)
            .await
        };
        let result =
            result.map_err(|e| ArcaError::Internal(format!("update_object_encryption_cas: {e}")))?;
        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("update_object_encryption_cas: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    // -- Lifecycle query operations --

    async fn list_expired_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        tags: &[(String, String)],
        cutoff: DateTime<Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        let mut sql = format!(
            "SELECT {OBJECT_COLUMNS} FROM objects \
             WHERE bucket = $1 AND is_latest = TRUE AND is_delete_marker = FALSE \
             AND last_modified < $2"
        );
        let mut param_idx = 3u32;

        let prefix_pattern = prefix.map(|p| {
            let idx = param_idx;
            param_idx += 1;
            sql.push_str(&format!(" AND key LIKE ${idx} ESCAPE '\\'"));
            format!("{}%", escape_like(p))
        });

        let has_start_after = start_after.is_some();
        if has_start_after {
            let idx = param_idx;
            param_idx += 1;
            sql.push_str(&format!(" AND key > ${idx}"));
        }

        // Tag filter: each tag requires an EXISTS subquery.
        let tags_vec: Vec<(String, String)> = tags.to_vec();
        for _ in &tags_vec {
            let key_idx = param_idx;
            param_idx += 1;
            let val_idx = param_idx;
            param_idx += 1;
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM object_tags \
                 WHERE object_tags.bucket = objects.bucket \
                 AND object_tags.key = objects.key \
                 AND object_tags.version_id = COALESCE(objects.version_id, '') \
                 AND object_tags.tag_key = ${key_idx} \
                 AND object_tags.tag_value = ${val_idx})"
            ));
        }

        sql.push_str(&format!(" ORDER BY key LIMIT {max_keys}"));

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str())).bind(bucket).bind(cutoff);
        if let Some(ref pattern) = prefix_pattern {
            query = query.bind(pattern.clone());
        }
        if let Some(sa) = start_after {
            query = query.bind(sa);
        }
        for (k, v) in &tags_vec {
            query = query.bind(k);
            query = query.bind(v);
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_expired_objects: {e}")))?;

        Ok(rows.iter().map(row_to_object_record).collect())
    }

    async fn list_noncurrent_expired_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        cutoff: DateTime<Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        let mut sql = format!(
            "SELECT {OBJECT_COLUMNS} FROM objects \
             WHERE bucket = $1 AND is_latest = FALSE AND is_delete_marker = FALSE AND is_tombstone = FALSE \
             AND last_modified < $2"
        );
        let mut param_idx = 3u32;

        let prefix_pattern = prefix.map(|p| {
            let idx = param_idx;
            param_idx += 1;
            sql.push_str(&format!(" AND key LIKE ${idx} ESCAPE '\\'"));
            format!("{}%", escape_like(p))
        });

        let has_start_after = start_after.is_some();
        if has_start_after {
            let idx = param_idx;
            #[allow(unused_assignments)]
            {
                param_idx += 1;
            }
            sql.push_str(&format!(" AND key > ${idx}"));
        }

        sql.push_str(&format!(
            " ORDER BY key, last_modified DESC LIMIT {max_keys}"
        ));

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str())).bind(bucket).bind(cutoff);
        if let Some(ref pattern) = prefix_pattern {
            query = query.bind(pattern.clone());
        }
        if let Some(sa) = start_after {
            query = query.bind(sa);
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| {
                ArcaError::Internal(format!("list_noncurrent_expired_versions: {e}"))
            })?;

        Ok(rows.iter().map(row_to_object_record).collect())
    }

    async fn list_stale_multipart_uploads(
        &self,
        bucket: &str,
        cutoff: DateTime<Utc>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        let sql = format!(
            "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm \
             FROM multipart_uploads \
             WHERE bucket = $1 AND initiated_at < $2 \
             ORDER BY key, upload_id \
             LIMIT {max_uploads}"
        );
        let rows = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()))
            .bind(bucket)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_stale_multipart_uploads: {e}")))?;

        Ok(rows.iter().map(row_to_multipart_upload_record).collect())
    }
}
