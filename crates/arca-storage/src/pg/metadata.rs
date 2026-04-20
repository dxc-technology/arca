//! PostgreSQL implementation of the `MetadataStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::MetadataStore;
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
    checksum_algorithm, checksum_value, replication_status";

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
    let row = sqlx_core::query::query(&sql)
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
        "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL"
    );
    let row = sqlx_core::query::query(&sql)
        .bind(bucket)
        .bind(key)
        .fetch_optional(&mut **tx)
        .await?;
    Ok(row.as_ref().map(row_to_object_record))
}

/// Inserts a new object row into the `objects` table.
async fn insert_object_row(
    tx: &mut sqlx_core::transaction::Transaction<'_, sqlx_postgres::Postgres>,
    record: &ObjectRecord,
    metadata_json: &serde_json::Value,
) -> Result<(), sqlx_core::error::Error> {
    sqlx_core::query::query(
        "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, \
         metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, \
         is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, \
         checksum_algorithm, checksum_value) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)",
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
fn row_to_multipart_upload_record(row: &sqlx_postgres::PgRow) -> MultipartUploadRecord {
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
fn row_to_part_record(row: &sqlx_postgres::PgRow) -> PartRecord {
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

    async fn put_object(
        &self,
        record: &ObjectRecord,
    ) -> Result<(Option<ObjectRecord>, Option<String>), ArcaError> {
        let mut record = record.clone();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

        let versioning = get_versioning_state(&mut tx, &record.bucket).await;

        let metadata_json = serde_json::to_value(&record.metadata)
            .unwrap_or_else(|_| serde_json::json!({}));

        let old = match versioning {
            VersioningState::Unversioned => {
                // Find old, DELETE + INSERT.
                let old = fetch_latest_object(&mut tx, &record.bucket, &record.key).await
                    .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                sqlx_core::query::query("DELETE FROM objects WHERE bucket = $1 AND key = $2")
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                // Clean up tags from overwritten object.
                sqlx_core::query::query("DELETE FROM object_tags WHERE bucket = $1 AND key = $2")
                    .bind(&record.bucket)
                    .bind(&record.key)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                record.version_id = None;
                record.is_latest = true;
                record.is_delete_marker = false;
                insert_object_row(&mut tx, &record, &metadata_json).await
                    .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

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
                .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                // Generate version ID and insert new row.
                record.version_id = Some(uuid::Uuid::new_v4().to_string());
                record.is_latest = true;
                record.is_delete_marker = false;
                insert_object_row(&mut tx, &record, &metadata_json).await
                    .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                None // keep old versions, no cleanup
            }
            VersioningState::Suspended => {
                // Delete existing null-version (if any) for cleanup.
                let old_null = fetch_null_version(&mut tx, &record.bucket, &record.key).await
                    .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                )
                .bind(&record.bucket)
                .bind(&record.key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                // Clean up tags from old null-version.
                sqlx_core::query::query(
                    "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = ''",
                )
                .bind(&record.bucket)
                .bind(&record.key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                // Mark any remaining latest as not-latest.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = FALSE \
                     WHERE bucket = $1 AND key = $2 AND is_latest = TRUE",
                )
                .bind(&record.bucket)
                .bind(&record.key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                // Insert with NULL version_id.
                record.version_id = None;
                record.is_latest = true;
                record.is_delete_marker = false;
                insert_object_row(&mut tx, &record, &metadata_json).await
                    .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

                old_null // clean up old null-version blob
            }
        };

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("put_object: {e}")))?;

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
        let row = sqlx_core::query::query(&sql)
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
        let row = sqlx_core::query::query(&sql)
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

        let mut query = sqlx_core::query::query(&sql).bind(bucket);
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

    async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

        let versioning = get_versioning_state(&mut tx, bucket).await;

        let result = match versioning {
            VersioningState::Unversioned => {
                // Hard-delete.
                let old = fetch_latest_object(&mut tx, bucket, key)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                if old.is_some() {
                    sqlx_core::query::query("DELETE FROM objects WHERE bucket = $1 AND key = $2")
                        .bind(bucket)
                        .bind(key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                    // Clean up tags.
                    sqlx_core::query::query("DELETE FROM object_tags WHERE bucket = $1 AND key = $2")
                        .bind(bucket)
                        .bind(key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;
                }

                old // blob to clean up
            }
            VersioningState::Enabled => {
                // Mark current latest as not-latest.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = FALSE \
                     WHERE bucket = $1 AND key = $2 AND is_latest = TRUE",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                // Insert a delete marker.
                let version_id = uuid::Uuid::new_v4().to_string();
                let now = Utc::now();
                let empty_metadata = serde_json::json!({});
                sqlx_core::query::query(
                    "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, \
                     last_modified, metadata, encryption_algorithm, encryption_key_id, owner, \
                     version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, \
                     legal_hold_status, storage_class, checksum_algorithm, checksum_value) \
                     VALUES ($1, $2, '', 0, '', NULL, $3, $4, NULL, NULL, 'root', $5, TRUE, TRUE, \
                     NULL, NULL, NULL, 'STANDARD', NULL, NULL)",
                )
                .bind(bucket)
                .bind(key)
                .bind(now)
                .bind(&empty_metadata)
                .bind(&version_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

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
                    retention_mode: None,
                    retain_until_date: None,
                    legal_hold_status: None,
                    storage_class: "STANDARD".to_string(),
                    checksum_algorithm: None,
                    checksum_value: None,
                    replication_status: None,
                })
            }
            VersioningState::Suspended => {
                // Delete existing null-version for cleanup.
                let old_null = fetch_null_version(&mut tx, bucket, key)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                // Clean up tags from old null-version.
                sqlx_core::query::query(
                    "DELETE FROM object_tags WHERE bucket = $1 AND key = $2 AND version_id = ''",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                // Mark any remaining latest as not-latest.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = FALSE \
                     WHERE bucket = $1 AND key = $2 AND is_latest = TRUE",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                // Insert delete marker with NULL version_id.
                let now = Utc::now();
                let empty_metadata = serde_json::json!({});
                sqlx_core::query::query(
                    "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, \
                     last_modified, metadata, encryption_algorithm, encryption_key_id, owner, \
                     version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, \
                     legal_hold_status, storage_class, checksum_algorithm, checksum_value) \
                     VALUES ($1, $2, '', 0, '', NULL, $3, $4, NULL, NULL, 'root', NULL, TRUE, TRUE, \
                     NULL, NULL, NULL, 'STANDARD', NULL, NULL)",
                )
                .bind(bucket)
                .bind(key)
                .bind(now)
                .bind(&empty_metadata)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

                old_null // clean up old null-version blob (if any)
            }
        };

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object: {e}")))?;

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
                 WHERE bucket = $1 AND key = $2 AND version_id IS NULL"
            );
            sqlx_core::query::query(&sql)
                .bind(bucket)
                .bind(key)
                .fetch_optional(&self.pool)
                .await
        } else {
            let sql = format!(
                "SELECT {OBJECT_COLUMNS} FROM objects \
                 WHERE bucket = $1 AND key = $2 AND version_id = $3"
            );
            sqlx_core::query::query(&sql)
                .bind(bucket)
                .bind(key)
                .bind(version_id)
                .fetch_optional(&self.pool)
                .await
        };

        let row = row.map_err(|e| ArcaError::Internal(format!("get_object_version: {e}")))?;
        Ok(row.as_ref().map(row_to_object_record))
    }

    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;

        // Fetch the version to delete.
        let deleted = if version_id == "null" {
            let sql = format!(
                "SELECT {OBJECT_COLUMNS} FROM objects \
                 WHERE bucket = $1 AND key = $2 AND version_id IS NULL"
            );
            let row = sqlx_core::query::query(&sql)
                .bind(bucket)
                .bind(key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;
            row.as_ref().map(row_to_object_record)
        } else {
            let sql = format!(
                "SELECT {OBJECT_COLUMNS} FROM objects \
                 WHERE bucket = $1 AND key = $2 AND version_id = $3"
            );
            let row = sqlx_core::query::query(&sql)
                .bind(bucket)
                .bind(key)
                .bind(version_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;
            row.as_ref().map(row_to_object_record)
        };

        if let Some(ref rec) = deleted {
            // Hard-delete the specific version.
            if version_id == "null" {
                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id IS NULL",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;
            } else {
                sqlx_core::query::query(
                    "DELETE FROM objects WHERE bucket = $1 AND key = $2 AND version_id = $3",
                )
                .bind(bucket)
                .bind(key)
                .bind(version_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;
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
            .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;

            // If deleted version was latest, promote next-newest.
            if rec.is_latest {
                // PostgreSQL doesn't have rowid, use ctid or a subquery approach.
                sqlx_core::query::query(
                    "UPDATE objects SET is_latest = TRUE WHERE ctid = (
                        SELECT ctid FROM objects WHERE bucket = $1 AND key = $2
                        ORDER BY last_modified DESC LIMIT 1
                    )",
                )
                .bind(bucket)
                .bind(key)
                .execute(&mut *tx)
                .await
                .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;
            }
        }

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_object_version: {e}")))?;

        Ok(deleted)
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
            "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = $1"
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

        let mut query = sqlx_core::query::query(&sql).bind(bucket);
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

        for (k, v) in tags {
            sqlx_core::query::query(
                "INSERT INTO bucket_tags (bucket, tag_key, tag_value) VALUES ($1, $2, $3)",
            )
            .bind(bucket)
            .bind(k)
            .bind(v)
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

        let mut query = sqlx_core::query::query(&sql).bind(bucket);
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

        let result = if let Some(vid) = version_id {
            sqlx_core::query::query(
                "UPDATE objects SET retention_mode = $1, retain_until_date = $2 \
                 WHERE bucket = $3 AND key = $4 AND version_id = $5",
            )
            .bind(retention_mode)
            .bind(retain_dt)
            .bind(bucket)
            .bind(key)
            .bind(vid)
            .execute(&self.pool)
            .await
        } else {
            sqlx_core::query::query(
                "UPDATE objects SET retention_mode = $1, retain_until_date = $2 \
                 WHERE bucket = $3 AND key = $4 AND is_latest = TRUE",
            )
            .bind(retention_mode)
            .bind(retain_dt)
            .bind(bucket)
            .bind(key)
            .execute(&self.pool)
            .await
        };

        let result =
            result.map_err(|e| ArcaError::Internal(format!("set_object_retention: {e}")))?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_object_legal_hold(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let result = if let Some(vid) = version_id {
            sqlx_core::query::query(
                "UPDATE objects SET legal_hold_status = $1 \
                 WHERE bucket = $2 AND key = $3 AND version_id = $4",
            )
            .bind(status)
            .bind(bucket)
            .bind(key)
            .bind(vid)
            .execute(&self.pool)
            .await
        } else {
            sqlx_core::query::query(
                "UPDATE objects SET legal_hold_status = $1 \
                 WHERE bucket = $2 AND key = $3 AND is_latest = TRUE",
            )
            .bind(status)
            .bind(bucket)
            .bind(key)
            .execute(&self.pool)
            .await
        };

        let result =
            result.map_err(|e| ArcaError::Internal(format!("set_object_legal_hold: {e}")))?;
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

        let mut query = sqlx_core::query::query(&sql).bind(bucket).bind(cutoff);
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
             WHERE bucket = $1 AND is_latest = FALSE AND is_delete_marker = FALSE \
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

        let mut query = sqlx_core::query::query(&sql).bind(bucket).bind(cutoff);
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
        let rows = sqlx_core::query::query(&sql)
            .bind(bucket)
            .bind(cutoff)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_stale_multipart_uploads: {e}")))?;

        Ok(rows.iter().map(row_to_multipart_upload_record).collect())
    }
}
