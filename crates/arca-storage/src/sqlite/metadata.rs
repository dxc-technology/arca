//! `MetadataStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::MetadataStore;
use arca_core::types::{
    BlobId, BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats,
    VersioningState,
};
use std::collections::HashMap;
use chrono::DateTime;
use rusqlite::{params, Connection};

use super::{SqliteStore, TrError};

/// Reads the bucket versioning state from `bucket_config`.
/// Called inside a synchronous `rusqlite::Connection` context.
fn get_versioning_state(conn: &Connection, bucket: &str) -> VersioningState {
    match conn.query_row(
        "SELECT config_value FROM bucket_config WHERE bucket = ?1 AND config_key = 'versioning'",
        params![bucket],
        |row| row.get::<_, String>(0),
    ) {
        Ok(ref v) if v == "Enabled" => VersioningState::Enabled,
        Ok(ref v) if v == "Suspended" => VersioningState::Suspended,
        _ => VersioningState::Unversioned,
    }
}

/// Column list for all object SELECT queries (20 columns).
const OBJECT_COLUMNS: &str = "bucket, key, blob_id, size, etag, content_type, last_modified, metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, checksum_algorithm, checksum_value, replication_status, is_tombstone, lock_updated_at";

#[async_trait::async_trait]
impl MetadataStore for SqliteStore {
    async fn get_stats(&self) -> Result<StorageStats, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let stats = conn.query_row(
                    "SELECT
                        (SELECT COUNT(*) FROM buckets),
                        (SELECT COUNT(*) FROM objects WHERE is_latest = 1 AND is_delete_marker = 0),
                        (SELECT COALESCE(SUM(size), 0) FROM objects WHERE is_latest = 1 AND is_delete_marker = 0)",
                    [],
                    |row| {
                        Ok(StorageStats {
                            bucket_count: row.get::<_, i64>(0)? as u64,
                            object_count: row.get::<_, i64>(1)? as u64,
                            total_size_bytes: row.get::<_, i64>(2)? as u64,
                        })
                    },
                )?;
                Ok(stats)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_stats: {e}")))
    }

    // -- Bucket operations --

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT name, created_at, owner FROM buckets ORDER BY name",
                )?;
                let rows = stmt.query_map([], |row| Ok(row_to_bucket_info(row)))?;
                let mut buckets = Vec::new();
                for row in rows {
                    buckets.push(row??);
                }
                Ok(buckets)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_buckets: {e}")))
    }

    async fn create_bucket(&self, name: &str) -> Result<(), ArcaError> {
        let name = name.to_string();
        let name_for_err = name.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO buckets (name, created_at) VALUES (?1, ?2)",
                    params![name, chrono::Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| {
                let msg = e.to_string();
                if msg.contains("UNIQUE constraint failed") {
                    ArcaError::S3(arca_core::S3Error::new(
                        arca_core::S3ErrorCode::BucketAlreadyOwnedByYou,
                        format!("/{name_for_err}"),
                    ))
                } else {
                    ArcaError::Internal(format!("create_bucket: {e}"))
                }
            })
    }

    async fn head_bucket(&self, name: &str) -> Result<Option<BucketInfo>, ArcaError> {
        let name = name.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT name, created_at, owner FROM buckets WHERE name = ?1",
                )?;
                let result = stmt.query_row(params![name], |row| Ok(row_to_bucket_info(row)));
                match result {
                    Ok(bucket) => Ok(Some(bucket?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("head_bucket: {e}")))
    }

    async fn delete_bucket(&self, name: &str) -> Result<bool, ArcaError> {
        let name = name.to_string();
        self.conn
            .call(move |conn| {
                // Clean up tags associated with this bucket
                conn.execute("DELETE FROM bucket_tags WHERE bucket = ?1", params![name])?;
                conn.execute("DELETE FROM object_tags WHERE bucket = ?1", params![name])?;
                let affected =
                    conn.execute("DELETE FROM buckets WHERE name = ?1", params![name])?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_bucket: {e}")))
    }

    async fn bucket_is_empty(&self, name: &str) -> Result<bool, ArcaError> {
        let name = name.to_string();
        self.read_conn()
            .call(move |conn| {
                let count: u32 = conn.query_row(
                    "SELECT COUNT(*) FROM objects WHERE bucket = ?1 LIMIT 1",
                    params![name],
                    |row| row.get(0),
                )?;
                Ok(count == 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("bucket_is_empty: {e}")))
    }

    // -- Object operations --

    async fn put_object(
        &self,
        record: &ObjectRecord,
    ) -> Result<(Option<ObjectRecord>, Option<String>), ArcaError> {
        let mut record = record.clone();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let versioning = get_versioning_state(&tx, &record.bucket);

                let metadata_json = serde_json::to_string(&record.metadata)
                    .unwrap_or_else(|_| "{}".to_string());

                let old = match versioning {
                    VersioningState::Unversioned => {
                        // Same as before: find old, DELETE + INSERT.
                        let old = fetch_latest_object(&tx, &record.bucket, &record.key)?;
                        tx.execute(
                            "DELETE FROM objects WHERE bucket = ?1 AND key = ?2",
                            params![record.bucket, record.key],
                        )?;
                        // Clean up tags from overwritten object
                        tx.execute(
                            "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2",
                            params![record.bucket, record.key],
                        )?;
                        record.version_id = None;
                        record.is_latest = true;
                        record.is_delete_marker = false;
                        insert_object_row(&tx, &record, &metadata_json)?;
                        // Finalize is_latest deterministically so the origin and
                        // cluster replicas (which run recompute in apply_remote_object)
                        // always agree on the current version (Phase 29, Risk #1).
                        recompute_is_latest(&tx, &record.bucket, &record.key)?;
                        tx.commit()?;
                        old // old blob to clean up
                    }
                    VersioningState::Enabled => {
                        // Mark current latest as not-latest.
                        tx.execute(
                            "UPDATE objects SET is_latest = 0 WHERE bucket = ?1 AND key = ?2 AND is_latest = 1",
                            params![record.bucket, record.key],
                        )?;
                        // Generate version ID and insert new row.
                        record.version_id = Some(uuid::Uuid::new_v4().to_string());
                        record.is_latest = true;
                        record.is_delete_marker = false;
                        insert_object_row(&tx, &record, &metadata_json)?;
                        // Finalize is_latest deterministically (see Risk #1 above).
                        recompute_is_latest(&tx, &record.bucket, &record.key)?;
                        tx.commit()?;
                        None // keep old versions, no cleanup
                    }
                    VersioningState::Suspended => {
                        // Delete existing null-version (if any) for cleanup.
                        let old_null = fetch_null_version(&tx, &record.bucket, &record.key)?;
                        tx.execute(
                            "DELETE FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL",
                            params![record.bucket, record.key],
                        )?;
                        // Clean up tags from old null-version
                        tx.execute(
                            "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2 AND version_id = ''",
                            params![record.bucket, record.key],
                        )?;
                        // Mark any remaining latest as not-latest.
                        tx.execute(
                            "UPDATE objects SET is_latest = 0 WHERE bucket = ?1 AND key = ?2 AND is_latest = 1",
                            params![record.bucket, record.key],
                        )?;
                        // Insert with NULL version_id.
                        record.version_id = None;
                        record.is_latest = true;
                        record.is_delete_marker = false;
                        insert_object_row(&tx, &record, &metadata_json)?;
                        // Finalize is_latest deterministically (see Risk #1 above).
                        recompute_is_latest(&tx, &record.bucket, &record.key)?;
                        tx.commit()?;
                        old_null // clean up old null-version blob
                    }
                };

                Ok((old, record.version_id.clone()))
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_object: {e}")))
    }

    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        self.read_conn()
            .call(move |conn| {
                let sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND is_latest = 1 AND is_delete_marker = 0"
                );
                let mut stmt = conn.prepare(&sql)?;
                let result = stmt.query_row(
                    params![bucket, key],
                    |row| Ok(row_to_object_record(row)),
                );
                match result {
                    Ok(rec) => Ok(Some(rec?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_object: {e}")))
    }

    async fn get_latest_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        self.read_conn()
            .call(move |conn| {
                let sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND is_latest = 1"
                );
                let mut stmt = conn.prepare(&sql)?;
                let result = stmt.query_row(
                    params![bucket, key],
                    |row| Ok(row_to_object_record(row)),
                );
                match result {
                    Ok(rec) => Ok(Some(rec?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_latest_object: {e}")))
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let prefix = prefix.map(|s| s.to_string());
        let start_after = start_after.map(|s| s.to_string());
        self.read_conn()
            .call(move |conn| {
                // Build dynamic SQL — only latest non-delete-marker objects.
                let mut sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND is_latest = 1 AND is_delete_marker = 0"
                );
                let mut param_idx = 2u32;

                let prefix_pattern = prefix.as_ref().map(|p| {
                    let idx = param_idx;
                    param_idx += 1;
                    sql.push_str(&format!(" AND key LIKE ?{idx} ESCAPE '\\'"));
                    format!("{}%", escape_like(p))
                });

                if start_after.is_some() {
                    let idx = param_idx;
                    #[allow(unused_assignments)]
                    { param_idx += 1; }
                    sql.push_str(&format!(" AND key > ?{idx}"));
                }

                sql.push_str(" ORDER BY key");
                sql.push_str(&format!(" LIMIT {max_keys}"));

                let mut stmt = conn.prepare(&sql)?;

                // Bind parameters dynamically.
                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                params_vec.push(Box::new(bucket));
                if let Some(ref pattern) = prefix_pattern {
                    params_vec.push(Box::new(pattern.clone()));
                }
                if let Some(ref sa) = start_after {
                    params_vec.push(Box::new(sa.clone()));
                }

                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                let rows = stmt.query_map(params_refs.as_slice(), |row| {
                    Ok(row_to_object_record(row))
                })?;

                let mut records = Vec::new();
                for row in rows {
                    records.push(row??);
                }
                Ok(records)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_objects: {e}")))
    }

    async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let cluster_mode = self.cluster_mode();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let versioning = get_versioning_state(&tx, &bucket);

                let result = match versioning {
                    VersioningState::Unversioned => {
                        let old = fetch_latest_object(&tx, &bucket, &key)?;
                        if old.is_some() {
                            // Clustered: leave a tombstone (blob cleared, fresh
                            // seq) so the deletion propagates via the manifest
                            // and anti-entropy can't resurrect it. Single-node:
                            // remove the row outright.
                            if cluster_mode {
                                let seq = next_object_seq(&tx)?;
                                tx.execute(
                                    "UPDATE objects SET is_tombstone = 1, is_delete_marker = 0, \
                                     blob_id = '', size = 0, last_modified = ?3, seq = ?4 \
                                     WHERE bucket = ?1 AND key = ?2",
                                    params![bucket, key, chrono::Utc::now().to_rfc3339(), seq],
                                )?;
                            } else {
                                tx.execute(
                                    "DELETE FROM objects WHERE bucket = ?1 AND key = ?2",
                                    params![bucket, key],
                                )?;
                            }
                            // Clean up tags either way.
                            tx.execute(
                                "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2",
                                params![bucket, key],
                            )?;
                            if cluster_mode {
                                recompute_is_latest(&tx, &bucket, &key)?;
                            }
                        }
                        tx.commit()?;
                        old // blob to clean up
                    }
                    VersioningState::Enabled => {
                        // Mark current latest as not-latest.
                        tx.execute(
                            "UPDATE objects SET is_latest = 0 WHERE bucket = ?1 AND key = ?2 AND is_latest = 1",
                            params![bucket, key],
                        )?;
                        // Insert a delete marker.
                        let version_id = uuid::Uuid::new_v4().to_string();
                        let now = chrono::Utc::now();
                        let seq = next_object_seq(&tx)?;
                        tx.execute(
                            "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, checksum_algorithm, checksum_value, seq, is_tombstone)
                             VALUES (?1, ?2, '', 0, '', NULL, ?3, '{}', NULL, NULL, 'root', ?4, 1, 1, NULL, NULL, NULL, 'STANDARD', NULL, NULL, ?5, 0)",
                            params![bucket, key, now.to_rfc3339(), version_id, seq],
                        )?;
                        tx.commit()?;
                        // Return the delete marker so the handler can set response headers.
                        Some(ObjectRecord {
                            bucket,
                            key,
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
                        })
                    }
                    VersioningState::Suspended => {
                        // Delete existing null-version for cleanup.
                        let old_null = fetch_null_version(&tx, &bucket, &key)?;
                        tx.execute(
                            "DELETE FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL",
                            params![bucket, key],
                        )?;
                        // Clean up tags from old null-version
                        tx.execute(
                            "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2 AND version_id = ''",
                            params![bucket, key],
                        )?;
                        // Mark any remaining latest as not-latest.
                        tx.execute(
                            "UPDATE objects SET is_latest = 0 WHERE bucket = ?1 AND key = ?2 AND is_latest = 1",
                            params![bucket, key],
                        )?;
                        // Insert delete marker with NULL version_id.
                        let now = chrono::Utc::now();
                        let seq = next_object_seq(&tx)?;
                        tx.execute(
                            "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, checksum_algorithm, checksum_value, seq, is_tombstone)
                             VALUES (?1, ?2, '', 0, '', NULL, ?3, '{}', NULL, NULL, 'root', NULL, 1, 1, NULL, NULL, NULL, 'STANDARD', NULL, NULL, ?4, 0)",
                            params![bucket, key, now.to_rfc3339(), seq],
                        )?;
                        tx.commit()?;
                        old_null // clean up old null-version blob (if any)
                    }
                };

                Ok(result)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_object: {e}")))
    }

    // -- Versioned object operations --

    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let sql = if version_id == "null" {
                    format!("SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL AND is_tombstone = 0")
                } else {
                    format!("SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id = ?3 AND is_tombstone = 0")
                };
                let mut stmt = conn.prepare(&sql)?;
                let result = if version_id == "null" {
                    stmt.query_row(params![bucket, key], |row| Ok(row_to_object_record(row)))
                } else {
                    stmt.query_row(params![bucket, key, version_id], |row| {
                        Ok(row_to_object_record(row))
                    })
                };
                match result {
                    Ok(rec) => Ok(Some(rec?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_object_version: {e}")))
    }

    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.to_string();
        let cluster_mode = self.cluster_mode();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;

                // Fetch the version to delete.
                let deleted = {
                    let sql = if version_id == "null" {
                        format!("SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL AND is_tombstone = 0")
                    } else {
                        format!("SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id = ?3 AND is_tombstone = 0")
                    };
                    let mut stmt = tx.prepare(&sql)?;
                    let result = if version_id == "null" {
                        stmt.query_row(params![bucket, key], |row| Ok(row_to_object_record(row)))
                    } else {
                        stmt.query_row(params![bucket, key, version_id], |row| {
                            Ok(row_to_object_record(row))
                        })
                    };
                    match result {
                        Ok(rec) => Some(rec?),
                        Err(rusqlite::Error::QueryReturnedNoRows) => None,
                        Err(e) => return Err(e.into()),
                    }
                };

                if let Some(ref rec) = deleted {
                    // Clustered: tombstone the version (blob cleared, fresh seq)
                    // so the deletion propagates via the manifest and can't be
                    // resurrected by anti-entropy. Single-node: remove the row.
                    if cluster_mode {
                        let seq = next_object_seq(&tx)?;
                        if version_id == "null" {
                            tx.execute(
                                "UPDATE objects SET is_tombstone = 1, is_delete_marker = 0, \
                                 blob_id = '', size = 0, last_modified = ?3, seq = ?4 \
                                 WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL",
                                params![bucket, key, chrono::Utc::now().to_rfc3339(), seq],
                            )?;
                        } else {
                            tx.execute(
                                "UPDATE objects SET is_tombstone = 1, is_delete_marker = 0, \
                                 blob_id = '', size = 0, last_modified = ?4, seq = ?5 \
                                 WHERE bucket = ?1 AND key = ?2 AND version_id = ?3",
                                params![bucket, key, version_id, chrono::Utc::now().to_rfc3339(), seq],
                            )?;
                        }
                    } else if version_id == "null" {
                        tx.execute(
                            "DELETE FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL",
                            params![bucket, key],
                        )?;
                    } else {
                        tx.execute(
                            "DELETE FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id = ?3",
                            params![bucket, key, version_id],
                        )?;
                    }

                    // Clean up tags for this version.
                    let tag_vid = if version_id == "null" { String::new() } else { version_id.clone() };
                    tx.execute(
                        "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2 AND version_id = ?3",
                        params![bucket, key, tag_vid],
                    )?;

                    // Recompute the latest version deterministically so the
                    // single-node and cluster (apply_remote_*) paths agree on
                    // the winner: (last_modified, version_id, blob_id) DESC.
                    let _ = rec;
                    recompute_is_latest(&tx, &bucket, &key)?;
                }

                tx.commit()?;
                Ok(deleted)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_object_version: {e}")))
    }

    async fn apply_remote_object(&self, record: &ObjectRecord) -> Result<(), ArcaError> {
        let record = record.clone();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let metadata_json =
                    serde_json::to_string(&record.metadata).unwrap_or_else(|_| "{}".to_string());

                match &record.version_id {
                    // Versioned rows are immutable, keyed by version_id, except
                    // for the lock columns (retention/legal hold), which mutate
                    // in place WITHOUT bumping last_modified. The LWW guard is
                    // therefore: strictly newer last_modified wins; on a tie —
                    // same version, possibly different lock state — strictly
                    // newer lock_updated_at wins (N1 delivery + N2 ordering).
                    // A plain `>=` here let the tie pass in BOTH directions: a
                    // node re-pulling a peer's full manifest after a restart
                    // re-applied the stale lock-free copy over a newer lock
                    // state, and the rewrite's fresh seq propagated the
                    // regression cluster-wide (N2). Identical rows are skipped
                    // without a rewrite (M7): rewriting would stamp a fresh
                    // seq and keep two caught-up nodes redelivering their
                    // whole tables to each other forever.
                    Some(vid) => {
                        let sql = format!(
                            "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id = ?3"
                        );
                        let existing: Option<ObjectRecord> = match tx.query_row(
                            &sql,
                            params![record.bucket, record.key, vid],
                            |row| Ok(row_to_object_record(row)),
                        ) {
                            Ok(rec) => Some(rec?),
                            Err(rusqlite::Error::QueryReturnedNoRows) => None,
                            Err(e) => return Err(e.into()),
                        };
                        let should_write = match &existing {
                            None => true,
                            Some(ex) => {
                                let newer = record.last_modified > ex.last_modified
                                    || (record.last_modified == ex.last_modified
                                        && record.lock_updated_at > ex.lock_updated_at);
                                newer && !record.same_replicated_content(ex)
                            }
                        };
                        if should_write {
                            tx.execute(
                                "DELETE FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id = ?3",
                                params![record.bucket, record.key, vid],
                            )?;
                            insert_replicated_row(&tx, &record, &metadata_json)?;
                        }
                    }
                    // Null-version rows form an LWW register per (bucket, key):
                    // unversioned/suspended overwrites resolve by
                    // (last_modified, blob_id), with lock_updated_at as the
                    // final tiebreak for the same physical row whose lock state
                    // changed in place (N1/N2 — see the versioned branch).
                    // Same M7 identical-row skip as the versioned branch.
                    None => {
                        let sql = format!(
                            "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL"
                        );
                        let existing: Option<ObjectRecord> = match tx.query_row(
                            &sql,
                            params![record.bucket, record.key],
                            |row| Ok(row_to_object_record(row)),
                        ) {
                            Ok(rec) => Some(rec?),
                            Err(rusqlite::Error::QueryReturnedNoRows) => None,
                            Err(e) => return Err(e.into()),
                        };
                        let should_write = match &existing {
                            None => true,
                            Some(ex) => {
                                // Tiebreak order on a last_modified tie:
                                // lock_updated_at first (so an in-place change
                                // that does not bump last_modified — a lock
                                // change OR a re-encryption, both of which stamp
                                // lock_updated_at — converges deterministically),
                                // then blob_id for two genuine concurrent
                                // overwrites that neither touched lock state
                                // (both lock_updated_at None). Mirrors the
                                // versioned branch's lock_updated_at tiebreak.
                                let lww = record.last_modified > ex.last_modified
                                    || (record.last_modified == ex.last_modified
                                        && record.lock_updated_at > ex.lock_updated_at)
                                    || (record.last_modified == ex.last_modified
                                        && record.lock_updated_at == ex.lock_updated_at
                                        && record.blob_id.0 > ex.blob_id.0);
                                lww && !record.same_replicated_content(ex)
                            }
                        };
                        if should_write {
                            tx.execute(
                                "DELETE FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL",
                                params![record.bucket, record.key],
                            )?;
                            insert_replicated_row(&tx, &record, &metadata_json)?;
                        }
                    }
                }

                recompute_is_latest(&tx, &record.bucket, &record.key)?;
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_remote_object: {e}")))
    }

    async fn apply_remote_version_delete(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.to_string();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                // Tombstone the version rather than removing it, so this node's
                // own manifest carries the deletion onward (transitive
                // convergence) and anti-entropy can't resurrect it. The
                // `is_tombstone = 0` guard makes re-delivery a true no-op (no
                // fresh seq, no re-propagation churn). Absent row → no-op; the
                // origin's tombstone still reaches this node via anti-entropy.
                let seq = next_object_seq(&tx)?;
                let now = chrono::Utc::now().to_rfc3339();
                if version_id == "null" {
                    tx.execute(
                        "UPDATE objects SET is_tombstone = 1, is_delete_marker = 0, blob_id = '', \
                         size = 0, last_modified = ?3, seq = ?4 \
                         WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL AND is_tombstone = 0",
                        params![bucket, key, now, seq],
                    )?;
                    tx.execute(
                        "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2 AND version_id = ''",
                        params![bucket, key],
                    )?;
                } else {
                    tx.execute(
                        "UPDATE objects SET is_tombstone = 1, is_delete_marker = 0, blob_id = '', \
                         size = 0, last_modified = ?4, seq = ?5 \
                         WHERE bucket = ?1 AND key = ?2 AND version_id = ?3 AND is_tombstone = 0",
                        params![bucket, key, version_id, now, seq],
                    )?;
                    tx.execute(
                        "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2 AND version_id = ?3",
                        params![bucket, key, version_id],
                    )?;
                }
                recompute_is_latest(&tx, &bucket, &key)?;
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("apply_remote_version_delete: {e}"))
            })
    }

    async fn apply_remote_bucket(&self, info: &BucketInfo) -> Result<(), ArcaError> {
        let name = info.name.clone();
        let created_at = info.created_at.to_rfc3339();
        let owner = info.owner.clone();
        self.conn
            .call(move |conn| {
                // ON CONFLICT DO UPDATE (not INSERT OR REPLACE) so we never
                // delete-and-reinsert the row, which could cascade to dependents.
                conn.execute(
                    "INSERT INTO buckets (name, created_at, owner) VALUES (?1, ?2, ?3) \
                     ON CONFLICT(name) DO UPDATE SET created_at = excluded.created_at, owner = excluded.owner",
                    params![name, created_at, owner],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_remote_bucket: {e}")))
    }

    async fn apply_remote_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        let record = record.clone();
        self.conn
            .call(move |conn| {
                let metadata_json =
                    serde_json::to_string(&record.metadata).unwrap_or_else(|_| "{}".to_string());
                // A multipart upload is immutable once created (upload_id is the
                // key), so a re-delivered op is a no-op rather than an error.
                conn.execute(
                    "INSERT INTO multipart_uploads (upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(upload_id) DO NOTHING",
                    params![
                        record.upload_id,
                        record.bucket,
                        record.key,
                        record.content_type,
                        record.initiated_at.to_rfc3339(),
                        metadata_json,
                        record.checksum_algorithm,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("apply_remote_multipart_upload: {e}"))
            })
    }

    async fn list_rows_changed_since(
        &self,
        since: u64,
        limit: u32,
    ) -> Result<Vec<(u64, ObjectRecord)>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                // `seq` is appended after the OBJECT_COLUMNS set, so it reads at
                // the index just past the record columns; row_to_object_record
                // stays untouched.
                let sql = format!(
                    "SELECT {OBJECT_COLUMNS}, seq FROM objects \
                     WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2"
                );
                let seq_idx = OBJECT_COLUMNS.split(',').count();
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![since as i64, limit], |row| {
                    let record = row_to_object_record(row)?;
                    let seq: i64 = row.get(seq_idx)?;
                    Ok((seq as u64, record))
                })?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_rows_changed_since: {e}")))
    }

    async fn purge_tombstones(
        &self,
        before: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, ArcaError> {
        let before_str = before.to_rfc3339();
        self.conn
            .call(move |conn| {
                let n = conn.execute(
                    "DELETE FROM objects WHERE is_tombstone = 1 AND last_modified < ?1",
                    params![before_str],
                )?;
                Ok(n as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("purge_tombstones: {e}")))
    }

    async fn current_object_seq(&self) -> Result<u64, ArcaError> {
        // The counter, not MAX(seq) over rows: purged tombstones make the row
        // maximum go backwards, which would false-alarm D3c rewind detection.
        self.read_conn()
            .call(move |conn| {
                let v: i64 = conn.query_row("SELECT value FROM object_seq", [], |row| row.get(0))?;
                Ok(v as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("current_object_seq: {e}")))
    }

    async fn list_referenced_blob_ids(&self) -> Result<Vec<BlobId>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT blob_id FROM objects WHERE is_tombstone = 0 AND blob_id != ''
                     UNION
                     SELECT blob_id FROM parts",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(BlobId(row?));
                }
                Ok(out)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_referenced_blob_ids: {e}")))
    }

    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        version_id_marker: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let prefix = prefix.map(|s| s.to_string());
        let key_marker = key_marker.map(|s| s.to_string());
        let _version_id_marker = version_id_marker.map(|s| s.to_string());
        self.read_conn()
            .call(move |conn| {
                let mut sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND is_tombstone = 0"
                );
                let mut param_idx = 2u32;

                let prefix_pattern = prefix.as_ref().map(|p| {
                    let idx = param_idx;
                    param_idx += 1;
                    sql.push_str(&format!(" AND key LIKE ?{idx} ESCAPE '\\'"));
                    format!("{}%", escape_like(p))
                });

                if let Some(ref km) = key_marker {
                    if !km.is_empty() {
                        let idx = param_idx;
                        #[allow(unused_assignments)]
                        { param_idx += 1; }
                        sql.push_str(&format!(" AND key > ?{idx}"));
                    }
                }

                sql.push_str(" ORDER BY key ASC, last_modified DESC");
                sql.push_str(&format!(" LIMIT {max_keys}"));

                let mut stmt = conn.prepare(&sql)?;

                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                params_vec.push(Box::new(bucket));
                if let Some(ref pattern) = prefix_pattern {
                    params_vec.push(Box::new(pattern.clone()));
                }
                if let Some(ref km) = key_marker {
                    if !km.is_empty() {
                        params_vec.push(Box::new(km.clone()));
                    }
                }

                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                let rows = stmt.query_map(params_refs.as_slice(), |row| {
                    Ok(row_to_object_record(row))
                })?;

                let mut records = Vec::new();
                for row in rows {
                    records.push(row??);
                }
                Ok(records)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_object_versions: {e}")))
    }

    // -- Bucket config operations --

    async fn get_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<Option<String>, ArcaError> {
        let bucket = bucket.to_string();
        let config_key = config_key.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT config_value FROM bucket_config WHERE bucket = ?1 AND config_key = ?2",
                )?;
                let result = stmt.query_row(params![bucket, config_key], |row| row.get(0));
                match result {
                    Ok(val) => Ok(Some(val)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_bucket_config: {e}")))
    }

    async fn set_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
    ) -> Result<(), ArcaError> {
        let bucket = bucket.to_string();
        let config_key = config_key.to_string();
        let config_value = config_value.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO bucket_config (bucket, config_key, config_value, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(bucket, config_key) DO UPDATE SET config_value = ?3, updated_at = ?4",
                    params![bucket, config_key, config_value, chrono::Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("set_bucket_config: {e}")))
    }

    async fn delete_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<bool, ArcaError> {
        let bucket = bucket.to_string();
        let config_key = config_key.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM bucket_config WHERE bucket = ?1 AND config_key = ?2",
                    params![bucket, config_key],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_bucket_config: {e}")))
    }

    async fn apply_bucket_config_at(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), ArcaError> {
        // Like set_bucket_config, but preserving the source's updated_at (the
        // R5 reconcile LWW key) instead of stamping now().
        let bucket = bucket.to_string();
        let config_key = config_key.to_string();
        let config_value = config_value.to_string();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO bucket_config (bucket, config_key, config_value, updated_at)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(bucket, config_key) DO UPDATE SET config_value = ?3, updated_at = ?4",
                    params![bucket, config_key, config_value, ts],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_bucket_config_at: {e}")))
    }

    async fn apply_bucket_tags_at(
        &self,
        bucket: &str,
        tags: &[(String, String)],
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), ArcaError> {
        // Like put_bucket_tags, but preserving the source's updated_at.
        let bucket = bucket.to_string();
        let tags = tags.to_vec();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                tx.execute("DELETE FROM bucket_tags WHERE bucket = ?1", params![bucket])?;
                for (k, v) in &tags {
                    tx.execute(
                        "INSERT INTO bucket_tags (bucket, tag_key, tag_value, updated_at) VALUES (?1, ?2, ?3, ?4)",
                        params![bucket, k, v, ts],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_bucket_tags_at: {e}")))
    }

    // -- Tag operations --

    async fn get_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<Vec<(String, String)>, ArcaError> {
        let bucket = bucket.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT tag_key, tag_value FROM bucket_tags WHERE bucket = ?1 ORDER BY tag_key",
                )?;
                let tags = stmt
                    .query_map(params![bucket], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(tags)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_bucket_tags: {e}")))
    }

    async fn put_bucket_tags(
        &self,
        bucket: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        let bucket = bucket.to_string();
        let tags = tags.to_vec();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                tx.execute("DELETE FROM bucket_tags WHERE bucket = ?1", params![bucket])?;
                // One timestamp for the whole set: the control reconcile (R5)
                // treats a bucket's tags as a single LWW entity (replace-all
                // semantics), keyed by MAX(updated_at) — equal here by design.
                let now = chrono::Utc::now().to_rfc3339();
                for (k, v) in &tags {
                    tx.execute(
                        "INSERT INTO bucket_tags (bucket, tag_key, tag_value, updated_at) VALUES (?1, ?2, ?3, ?4)",
                        params![bucket, k, v, now],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_bucket_tags: {e}")))
    }

    async fn delete_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<bool, ArcaError> {
        let bucket = bucket.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM bucket_tags WHERE bucket = ?1",
                    params![bucket],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_bucket_tags: {e}")))
    }

    async fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Vec<(String, String)>, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT tag_key, tag_value FROM object_tags
                     WHERE bucket = ?1 AND key = ?2 AND version_id = ?3
                     ORDER BY tag_key",
                )?;
                let tags = stmt
                    .query_map(params![bucket, key, version_id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(tags)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_object_tags: {e}")))
    }

    async fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.to_string();
        let tags = tags.to_vec();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2 AND version_id = ?3",
                    params![bucket, key, version_id],
                )?;
                for (k, v) in &tags {
                    tx.execute(
                        "INSERT INTO object_tags (bucket, key, version_id, tag_key, tag_value)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![bucket, key, version_id, k, v],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_object_tags: {e}")))
    }

    async fn delete_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<bool, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM object_tags WHERE bucket = ?1 AND key = ?2 AND version_id = ?3",
                    params![bucket, key, version_id],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_object_tags: {e}")))
    }

    // -- Multipart upload operations --

    async fn create_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        let record = record.clone();
        self.conn
            .call(move |conn| {
                let metadata_json = serde_json::to_string(&record.metadata)
                    .unwrap_or_else(|_| "{}".to_string());
                conn.execute(
                    "INSERT INTO multipart_uploads (upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        record.upload_id,
                        record.bucket,
                        record.key,
                        record.content_type,
                        record.initiated_at.to_rfc3339(),
                        metadata_json,
                        record.checksum_algorithm,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("create_multipart_upload: {e}")))
    }

    async fn get_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Option<MultipartUploadRecord>, ArcaError> {
        let upload_id = upload_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm
                     FROM multipart_uploads WHERE upload_id = ?1",
                )?;
                let result = stmt.query_row(params![upload_id], |row| {
                    Ok(row_to_multipart_upload_record(row))
                });
                match result {
                    Ok(rec) => Ok(Some(rec?)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_multipart_upload: {e}")))
    }

    async fn put_part(&self, part: &PartRecord) -> Result<Option<PartRecord>, ArcaError> {
        let part = part.clone();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;

                // Check for existing part to return for cleanup.
                let old = {
                    let mut stmt = tx.prepare(
                        "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified
                         FROM parts WHERE upload_id = ?1 AND part_number = ?2",
                    )?;
                    let result = stmt.query_row(
                        params![part.upload_id, part.part_number],
                        |row| Ok(row_to_part_record(row)),
                    );
                    match result {
                        Ok(rec) => Some(rec?),
                        Err(rusqlite::Error::QueryReturnedNoRows) => None,
                        Err(e) => return Err(e.into()),
                    }
                };

                // Delete old part if exists, then insert new.
                tx.execute(
                    "DELETE FROM parts WHERE upload_id = ?1 AND part_number = ?2",
                    params![part.upload_id, part.part_number],
                )?;
                let last_modified_str = part.last_modified.map(|dt| dt.to_rfc3339());
                tx.execute(
                    "INSERT INTO parts (upload_id, part_number, blob_id, size, etag, checksum_value, last_modified)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        part.upload_id,
                        part.part_number,
                        part.blob_id.0,
                        part.size as i64,
                        part.etag,
                        part.checksum_value,
                        last_modified_str,
                    ],
                )?;

                tx.commit()?;
                Ok(old)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("put_part: {e}")))
    }

    async fn list_parts(&self, upload_id: &str) -> Result<Vec<PartRecord>, ArcaError> {
        let upload_id = upload_id.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified
                     FROM parts WHERE upload_id = ?1 ORDER BY part_number",
                )?;
                let rows = stmt.query_map(params![upload_id], |row| {
                    Ok(row_to_part_record(row))
                })?;
                let mut parts = Vec::new();
                for row in rows {
                    parts.push(row??);
                }
                Ok(parts)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_parts: {e}")))
    }

    async fn delete_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, ArcaError> {
        let upload_id = upload_id.to_string();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;

                // Collect parts for blob cleanup.
                let parts = {
                    let mut stmt = tx.prepare(
                        "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified
                         FROM parts WHERE upload_id = ?1",
                    )?;
                    let rows = stmt.query_map(params![upload_id], |row| {
                        Ok(row_to_part_record(row))
                    })?;
                    let mut parts = Vec::new();
                    for row in rows {
                        parts.push(row??);
                    }
                    parts
                };

                // Delete parts and upload record.
                tx.execute(
                    "DELETE FROM parts WHERE upload_id = ?1",
                    params![upload_id],
                )?;
                tx.execute(
                    "DELETE FROM multipart_uploads WHERE upload_id = ?1",
                    params![upload_id],
                )?;

                tx.commit()?;
                Ok(parts)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_multipart_upload: {e}")))
    }

    async fn list_multipart_uploads(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        upload_id_marker: Option<&str>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let prefix = prefix.map(|s| s.to_string());
        let key_marker = key_marker.map(|s| s.to_string());
        let upload_id_marker = upload_id_marker.map(|s| s.to_string());
        self.read_conn()
            .call(move |conn| {
                let mut sql = String::from(
                    "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm
                     FROM multipart_uploads WHERE bucket = ?1",
                );
                let mut param_idx = 2u32;

                let prefix_pattern = prefix.as_ref().map(|p| {
                    let idx = param_idx;
                    param_idx += 1;
                    sql.push_str(&format!(" AND key LIKE ?{idx} ESCAPE '\\'"));
                    format!("{}%", escape_like(p))
                });

                // Pagination: key_marker + upload_id_marker
                if let Some(ref km) = key_marker {
                    if let Some(ref uim) = upload_id_marker {
                        if !uim.is_empty() {
                            let kid = param_idx;
                            param_idx += 1;
                            let uid = param_idx;
                            #[allow(unused_assignments)]
                            { param_idx += 1; }
                            sql.push_str(&format!(
                                " AND (key > ?{kid} OR (key = ?{kid} AND upload_id > ?{uid}))"
                            ));
                            // We'll bind km twice and uim once — handled below.
                        } else if !km.is_empty() {
                            let idx = param_idx;
                            #[allow(unused_assignments)]
                            { param_idx += 1; }
                            sql.push_str(&format!(" AND key > ?{idx}"));
                        }
                    } else if !km.is_empty() {
                        let idx = param_idx;
                        #[allow(unused_assignments)]
                        { param_idx += 1; }
                        sql.push_str(&format!(" AND key > ?{idx}"));
                    }
                }

                sql.push_str(" ORDER BY key, upload_id");
                sql.push_str(&format!(" LIMIT {max_uploads}"));

                let mut stmt = conn.prepare(&sql)?;

                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                params_vec.push(Box::new(bucket));
                if let Some(ref pattern) = prefix_pattern {
                    params_vec.push(Box::new(pattern.clone()));
                }
                if let Some(ref km) = key_marker {
                    if let Some(ref uim) = upload_id_marker {
                        if !uim.is_empty() {
                            params_vec.push(Box::new(km.clone()));
                            params_vec.push(Box::new(uim.clone()));
                        } else if !km.is_empty() {
                            params_vec.push(Box::new(km.clone()));
                        }
                    } else if !km.is_empty() {
                        params_vec.push(Box::new(km.clone()));
                    }
                }

                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                let rows = stmt.query_map(params_refs.as_slice(), |row| {
                    Ok(row_to_multipart_upload_record(row))
                })?;

                let mut records = Vec::new();
                for row in rows {
                    records.push(row??);
                }
                Ok(records)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_multipart_uploads: {e}")))
    }

    async fn set_object_retention(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        retention_mode: Option<&str>,
        retain_until_date: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.map(|s| s.to_string());
        let retention_mode = retention_mode.map(|s| s.to_string());
        let retain_until_date = retain_until_date.map(|s| s.to_string());

        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                // Fresh seq so the lock change travels via the changed-since
                // manifest to peers that miss the real-time fan-out (N1). Taken
                // before the row UPDATE per the next_object_seq lock-order rule.
                // lock_updated_at orders the lock state across nodes (N2): the
                // row's last_modified does not change here, so without it a
                // stale equal-timestamp copy applied later would clobber this.
                let seq = next_object_seq(&tx)?;
                let lock_updated_at = chrono::Utc::now().to_rfc3339();
                let rows = if let Some(ref vid) = version_id {
                    tx.execute(
                        "UPDATE objects SET retention_mode = ?1, retain_until_date = ?2, seq = ?3, lock_updated_at = ?4 \
                         WHERE bucket = ?5 AND key = ?6 AND version_id = ?7",
                        params![retention_mode, retain_until_date, seq, lock_updated_at, bucket, key, vid],
                    )?
                } else {
                    tx.execute(
                        "UPDATE objects SET retention_mode = ?1, retain_until_date = ?2, seq = ?3, lock_updated_at = ?4 \
                         WHERE bucket = ?5 AND key = ?6 AND is_latest = 1",
                        params![retention_mode, retain_until_date, seq, lock_updated_at, bucket, key],
                    )?
                };
                tx.commit()?;
                Ok(rows > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("set_object_retention: {e}")))
    }

    async fn set_object_legal_hold(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.map(|s| s.to_string());
        let status = status.map(|s| s.to_string());

        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                // Fresh seq for manifest visibility (N1) and lock_updated_at
                // for the lock-state LWW (N2) — see set_object_retention.
                let seq = next_object_seq(&tx)?;
                let lock_updated_at = chrono::Utc::now().to_rfc3339();
                let rows = if let Some(ref vid) = version_id {
                    tx.execute(
                        "UPDATE objects SET legal_hold_status = ?1, seq = ?2, lock_updated_at = ?3 \
                         WHERE bucket = ?4 AND key = ?5 AND version_id = ?6",
                        params![status, seq, lock_updated_at, bucket, key, vid],
                    )?
                } else {
                    tx.execute(
                        "UPDATE objects SET legal_hold_status = ?1, seq = ?2, lock_updated_at = ?3 \
                         WHERE bucket = ?4 AND key = ?5 AND is_latest = 1",
                        params![status, seq, lock_updated_at, bucket, key],
                    )?
                };
                tx.commit()?;
                Ok(rows > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("set_object_legal_hold: {e}")))
    }

    async fn update_object_encryption(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        algorithm: Option<&str>,
        key_id: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.map(|s| s.to_string());
        let algorithm = algorithm.map(|s| s.to_string());
        let key_id = key_id.map(|s| s.to_string());

        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                // Fresh seq so the re-encryption travels to peers via the
                // changed-since manifest; last_modified/ETag are untouched (the
                // logical object is unchanged). A fresh lock_updated_at is the
                // cluster LWW convergence dimension: re-encryption is an
                // in-place row change that does NOT bump last_modified, so peers
                // must adopt it via lock_updated_at (same mechanism as a lock
                // change — see apply_remote_object). Seq taken before the row
                // UPDATE per the next_object_seq lock-order rule.
                let seq = next_object_seq(&tx)?;
                let lock_updated_at = chrono::Utc::now().to_rfc3339();
                let rows = if let Some(ref vid) = version_id {
                    tx.execute(
                        "UPDATE objects SET encryption_algorithm = ?1, encryption_key_id = ?2, seq = ?3, lock_updated_at = ?4 \
                         WHERE bucket = ?5 AND key = ?6 AND version_id = ?7",
                        params![algorithm, key_id, seq, lock_updated_at, bucket, key, vid],
                    )?
                } else {
                    tx.execute(
                        "UPDATE objects SET encryption_algorithm = ?1, encryption_key_id = ?2, seq = ?3, lock_updated_at = ?4 \
                         WHERE bucket = ?5 AND key = ?6 AND is_latest = 1",
                        params![algorithm, key_id, seq, lock_updated_at, bucket, key],
                    )?
                };
                tx.commit()?;
                Ok(rows > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_object_encryption: {e}")))
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
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.map(|s| s.to_string());
        let old_blob_id = old_blob_id.0.clone();
        let new_blob_id = new_blob_id.0.clone();
        let algorithm = algorithm.map(|s| s.to_string());
        let key_id = key_id.map(|s| s.to_string());

        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                // CAS guard on blob_id: a concurrent client overwrite already
                // swapped the blob, in which case 0 rows match and the caller
                // discards the freshly written blob. A fresh lock_updated_at is
                // the cluster LWW convergence dimension (see update_object_encryption).
                let seq = next_object_seq(&tx)?;
                let lock_updated_at = chrono::Utc::now().to_rfc3339();
                let rows = if let Some(ref vid) = version_id {
                    tx.execute(
                        "UPDATE objects SET blob_id = ?1, encryption_algorithm = ?2, encryption_key_id = ?3, seq = ?4, lock_updated_at = ?5 \
                         WHERE bucket = ?6 AND key = ?7 AND version_id = ?8 AND blob_id = ?9",
                        params![new_blob_id, algorithm, key_id, seq, lock_updated_at, bucket, key, vid, old_blob_id],
                    )?
                } else {
                    tx.execute(
                        "UPDATE objects SET blob_id = ?1, encryption_algorithm = ?2, encryption_key_id = ?3, seq = ?4, lock_updated_at = ?5 \
                         WHERE bucket = ?6 AND key = ?7 AND is_latest = 1 AND blob_id = ?8",
                        params![new_blob_id, algorithm, key_id, seq, lock_updated_at, bucket, key, old_blob_id],
                    )?
                };
                tx.commit()?;
                Ok(rows > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_object_encryption_cas: {e}")))
    }

    async fn list_expired_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        tags: &[(String, String)],
        cutoff: chrono::DateTime<chrono::Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let prefix = prefix.map(|s| s.to_string());
        let tags: Vec<(String, String)> = tags.to_vec();
        let cutoff_str = cutoff.to_rfc3339();
        let start_after = start_after.map(|s| s.to_string());

        self.read_conn()
            .call(move |conn| {
                let mut sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects \
                     WHERE bucket = ?1 AND is_latest = 1 AND is_delete_marker = 0 AND last_modified < ?2"
                );
                let mut param_idx = 3u32;

                let prefix_pattern = prefix.as_ref().map(|p| {
                    let idx = param_idx;
                    param_idx += 1;
                    sql.push_str(&format!(" AND key LIKE ?{idx} ESCAPE '\\'"));
                    format!("{}%", escape_like(p))
                });

                let start_after_idx = start_after.as_ref().map(|_| {
                    let idx = param_idx;
                    param_idx += 1;
                    sql.push_str(&format!(" AND key > ?{idx}"));
                    idx
                });

                // Tag filter: each tag requires an EXISTS subquery
                let mut tag_indices = Vec::new();
                for _ in &tags {
                    let key_idx = param_idx;
                    param_idx += 1;
                    let val_idx = param_idx;
                    param_idx += 1;
                    sql.push_str(&format!(
                        " AND EXISTS (SELECT 1 FROM object_tags \
                         WHERE object_tags.bucket = objects.bucket \
                         AND object_tags.key = objects.key \
                         AND object_tags.version_id = COALESCE(objects.version_id, '') \
                         AND object_tags.tag_key = ?{key_idx} \
                         AND object_tags.tag_value = ?{val_idx})"
                    ));
                    tag_indices.push((key_idx, val_idx));
                }

                sql.push_str(&format!(" ORDER BY key LIMIT {max_keys}"));

                let mut stmt = conn.prepare(&sql)?;

                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                params_vec.push(Box::new(bucket));
                params_vec.push(Box::new(cutoff_str));
                if let Some(ref pattern) = prefix_pattern {
                    params_vec.push(Box::new(pattern.clone()));
                }
                if let Some(_) = start_after_idx {
                    params_vec.push(Box::new(start_after.unwrap()));
                }
                for (i, (key, value)) in tags.iter().enumerate() {
                    let _ = tag_indices[i]; // indices match
                    params_vec.push(Box::new(key.clone()));
                    params_vec.push(Box::new(value.clone()));
                }

                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                let rows = stmt.query_map(params_refs.as_slice(), |row| {
                    Ok(row_to_object_record(row))
                })?;

                let mut records = Vec::new();
                for row in rows {
                    records.push(row??);
                }
                Ok(records)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_expired_objects: {e}")))
    }

    async fn list_noncurrent_expired_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        cutoff: chrono::DateTime<chrono::Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let prefix = prefix.map(|s| s.to_string());
        let cutoff_str = cutoff.to_rfc3339();
        let start_after = start_after.map(|s| s.to_string());

        self.read_conn()
            .call(move |conn| {
                let mut sql = format!(
                    "SELECT {OBJECT_COLUMNS} FROM objects \
                     WHERE bucket = ?1 AND is_latest = 0 AND is_delete_marker = 0 AND is_tombstone = 0 AND last_modified < ?2"
                );
                let mut param_idx = 3u32;

                let prefix_pattern = prefix.as_ref().map(|p| {
                    let idx = param_idx;
                    param_idx += 1;
                    sql.push_str(&format!(" AND key LIKE ?{idx} ESCAPE '\\'"));
                    format!("{}%", escape_like(p))
                });

                let start_after_idx = start_after.as_ref().map(|_| {
                    let idx = param_idx;
                    #[allow(unused_assignments)]
                    { param_idx += 1; }
                    sql.push_str(&format!(" AND key > ?{idx}"));
                    idx
                });

                sql.push_str(&format!(" ORDER BY key, last_modified DESC LIMIT {max_keys}"));

                let mut stmt = conn.prepare(&sql)?;

                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                params_vec.push(Box::new(bucket));
                params_vec.push(Box::new(cutoff_str));
                if let Some(ref pattern) = prefix_pattern {
                    params_vec.push(Box::new(pattern.clone()));
                }
                if let Some(_) = start_after_idx {
                    params_vec.push(Box::new(start_after.unwrap()));
                }

                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                let rows = stmt.query_map(params_refs.as_slice(), |row| {
                    Ok(row_to_object_record(row))
                })?;

                let mut records = Vec::new();
                for row in rows {
                    records.push(row??);
                }
                Ok(records)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_noncurrent_expired_versions: {e}")))
    }

    async fn list_stale_multipart_uploads(
        &self,
        bucket: &str,
        cutoff: chrono::DateTime<chrono::Utc>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let cutoff_str = cutoff.to_rfc3339();

        self.read_conn()
            .call(move |conn| {
                let sql = format!(
                    "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm \
                     FROM multipart_uploads \
                     WHERE bucket = ?1 AND initiated_at < ?2 \
                     ORDER BY key, upload_id \
                     LIMIT {max_uploads}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![bucket, cutoff_str], |row| {
                    Ok(row_to_multipart_upload_record(row))
                })?;

                let mut records = Vec::new();
                for row in rows {
                    records.push(row??);
                }
                Ok(records)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_stale_multipart_uploads: {e}")))
    }
}

/// Fetches the latest object version (the row with `is_latest=1`) for a given bucket/key.
/// Used by `put_object` (unversioned) and `delete_object` (unversioned) to find the
/// record to return for blob cleanup.
fn fetch_latest_object(
    conn: &Connection,
    bucket: &str,
    key: &str,
) -> Result<Option<ObjectRecord>, rusqlite::Error> {
    let sql = format!(
        "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND is_latest = 1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let result = stmt.query_row(params![bucket, key], |row| Ok(row_to_object_record(row)));
    match result {
        Ok(rec) => Ok(Some(rec?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Fetches the null-version row (version_id IS NULL) for a given bucket/key.
/// Used in suspended-mode operations to find the record to clean up.
fn fetch_null_version(
    conn: &Connection,
    bucket: &str,
    key: &str,
) -> Result<Option<ObjectRecord>, rusqlite::Error> {
    let sql = format!(
        "SELECT {OBJECT_COLUMNS} FROM objects WHERE bucket = ?1 AND key = ?2 AND version_id IS NULL AND is_tombstone = 0"
    );
    let mut stmt = conn.prepare(&sql)?;
    let result = stmt.query_row(params![bucket, key], |row| Ok(row_to_object_record(row)));
    match result {
        Ok(rec) => Ok(Some(rec?)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Returns the next node-local monotonic `seq`, advancing the dedicated
/// `object_seq` counter. Stamped on every local write — including
/// `apply_remote_object` — so a peer's changed-since manifest pulls exactly the
/// rows this node wrote since its last visit.
///
/// A standalone counter, NOT `MAX(seq) + 1`: rewriting the highest-seq object
/// DELETEs then re-INSERTs it, which would drop `MAX(seq)` below a caught-up
/// peer's cursor and hide the rewrite. The counter only ever increases. Monotonic
/// without locking because the tokio-rusqlite connection serializes all writes
/// on one thread; the caller runs this inside its transaction.
fn next_object_seq(conn: &Connection) -> Result<i64, rusqlite::Error> {
    conn.execute("UPDATE object_seq SET value = value + 1", [])?;
    conn.query_row("SELECT value FROM object_seq", [], |row| row.get(0))
}

/// Inserts a new object row into the `objects` table.
fn insert_object_row(
    conn: &Connection,
    record: &ObjectRecord,
    metadata_json: &str,
) -> Result<(), rusqlite::Error> {
    let retain_until_str = record.retain_until_date.map(|dt| dt.to_rfc3339());
    let lock_updated_str = record.lock_updated_at.map(|dt| dt.to_rfc3339());
    let seq = next_object_seq(conn)?;
    conn.execute(
        "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, checksum_algorithm, checksum_value, seq, is_tombstone, lock_updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        params![
            record.bucket,
            record.key,
            record.blob_id.0,
            record.size as i64,
            record.etag,
            record.content_type,
            record.last_modified.to_rfc3339(),
            metadata_json,
            record.encryption_algorithm,
            record.encryption_key_id,
            record.owner,
            record.version_id,
            record.is_latest as i32,
            record.is_delete_marker as i32,
            record.retention_mode,
            retain_until_str,
            record.legal_hold_status,
            record.storage_class,
            record.checksum_algorithm,
            record.checksum_value,
            seq,
            record.is_tombstone as i32,
            lock_updated_str,
        ],
    )?;
    Ok(())
}

/// Recomputes `is_latest` for a key deterministically: exactly the row with the
/// greatest `(last_modified, version_id, blob_id)` is marked latest, all others
/// not. Shared by the cluster apply paths and `delete_object_version` so every
/// node converges on the same current version without coordination. Respects
/// the unique index `idx_objects_latest` by zeroing all rows before setting the
/// single winner.
///
/// Tombstones (hard-deleted versions kept only for cluster convergence) are
/// EXCLUDED from the winner pick, so a tombstone is never `is_latest` and a
/// query filtering `is_latest = 1` transparently skips deleted rows. Deleting
/// the latest version thus promotes the next live version; deleting the only
/// version leaves no `is_latest` row (the key reads as absent).
fn recompute_is_latest(conn: &Connection, bucket: &str, key: &str) -> Result<(), rusqlite::Error> {
    conn.execute(
        "UPDATE objects SET is_latest = 0 WHERE bucket = ?1 AND key = ?2",
        params![bucket, key],
    )?;
    conn.execute(
        "UPDATE objects SET is_latest = 1 WHERE rowid = (
            SELECT rowid FROM objects WHERE bucket = ?1 AND key = ?2 AND is_tombstone = 0
            ORDER BY last_modified DESC, version_id DESC, blob_id DESC
            LIMIT 1
        )",
        params![bucket, key],
    )?;
    Ok(())
}

/// Inserts a replicated object row verbatim, including `replication_status`.
/// `is_latest` is forced to 0 on insert so the unique latest index is never
/// transiently violated; the caller then runs `recompute_is_latest`.
fn insert_replicated_row(
    conn: &Connection,
    record: &ObjectRecord,
    metadata_json: &str,
) -> Result<(), rusqlite::Error> {
    let retain_until_str = record.retain_until_date.map(|dt| dt.to_rfc3339());
    let lock_updated_str = record.lock_updated_at.map(|dt| dt.to_rfc3339());
    let seq = next_object_seq(conn)?;
    conn.execute(
        "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, metadata, encryption_algorithm, encryption_key_id, owner, version_id, is_latest, is_delete_marker, retention_mode, retain_until_date, legal_hold_status, storage_class, checksum_algorithm, checksum_value, replication_status, seq, is_tombstone, lock_updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        params![
            record.bucket,
            record.key,
            record.blob_id.0,
            record.size as i64,
            record.etag,
            record.content_type,
            record.last_modified.to_rfc3339(),
            metadata_json,
            record.encryption_algorithm,
            record.encryption_key_id,
            record.owner,
            record.version_id,
            record.is_delete_marker as i32,
            record.retention_mode,
            retain_until_str,
            record.legal_hold_status,
            record.storage_class,
            record.checksum_algorithm,
            record.checksum_value,
            record.replication_status,
            seq,
            record.is_tombstone as i32,
            lock_updated_str,
        ],
    )?;
    Ok(())
}

/// Escapes special characters in a LIKE pattern so they are matched literally.
///
/// SQLite LIKE special characters are `%` and `_`. We use `\` as the escape character.
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

/// Converts a SQLite row to a `BucketInfo`.
///
/// Expects columns: name, created_at, owner.
fn row_to_bucket_info(row: &rusqlite::Row) -> Result<BucketInfo, rusqlite::Error> {
    let created_at_str: String = row.get(1)?;
    let created_at = DateTime::parse_from_rfc3339(&created_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    Ok(BucketInfo {
        name: row.get(0)?,
        created_at,
        owner: row.get(2)?,
    })
}

/// Converts a SQLite row to a `MultipartUploadRecord`.
///
/// Expects columns: upload_id, bucket, key, content_type, initiated_at, metadata.
/// `pub(super)`: the control-snapshot builder reads these tables too (R5/D4).
pub(super) fn row_to_multipart_upload_record(
    row: &rusqlite::Row,
) -> Result<MultipartUploadRecord, rusqlite::Error> {
    let initiated_at_str: String = row.get(4)?;
    let initiated_at = DateTime::parse_from_rfc3339(&initiated_at_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    let metadata_json: String = row.get(5)?;
    let metadata: HashMap<String, String> =
        serde_json::from_str(&metadata_json).unwrap_or_default();

    let checksum_algorithm: Option<String> = row.get(6).unwrap_or(None);

    Ok(MultipartUploadRecord {
        upload_id: row.get(0)?,
        bucket: row.get(1)?,
        key: row.get(2)?,
        content_type: row.get(3)?,
        initiated_at,
        metadata,
        checksum_algorithm,
    })
}

/// Converts a SQLite row to a `PartRecord`.
///
/// Expects columns: upload_id, part_number, blob_id, size, etag, checksum_value, last_modified.
/// `pub(super)`: the control-snapshot builder reads these tables too (R5/D4).
pub(super) fn row_to_part_record(row: &rusqlite::Row) -> Result<PartRecord, rusqlite::Error> {
    let checksum_value: Option<String> = row.get(5).unwrap_or(None);
    let last_modified_str: Option<String> = row.get(6).unwrap_or(None);
    let last_modified = last_modified_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .ok()
    });
    Ok(PartRecord {
        upload_id: row.get(0)?,
        part_number: row.get(1)?,
        blob_id: BlobId(row.get(2)?),
        size: row.get::<_, i64>(3)? as u64,
        etag: row.get(4)?,
        checksum_value,
        last_modified,
    })
}

/// Converts a SQLite row to an `ObjectRecord`.
///
/// Expects columns: bucket, key, blob_id, size, etag, content_type, last_modified, metadata,
/// encryption_algorithm, encryption_key_id, owner, version_id, is_latest, is_delete_marker.
fn row_to_object_record(row: &rusqlite::Row) -> Result<ObjectRecord, rusqlite::Error> {
    let last_modified_str: String = row.get(6)?;
    let last_modified = DateTime::parse_from_rfc3339(&last_modified_str)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Text,
                Box::new(e),
            )
        })?;

    let metadata_json: String = row.get(7)?;
    let metadata: HashMap<String, String> =
        serde_json::from_str(&metadata_json).unwrap_or_default();

    let encryption_algorithm: Option<String> = row.get(8)?;
    let encryption_key_id: Option<String> = row.get(9)?;

    let owner: String = row.get(10).unwrap_or_else(|_| "root".to_string());

    let version_id: Option<String> = row.get(11)?;
    let is_latest: bool = row.get::<_, i32>(12).unwrap_or(1) != 0;
    let is_delete_marker: bool = row.get::<_, i32>(13).unwrap_or(0) != 0;

    let retention_mode: Option<String> = row.get(14).unwrap_or(None);
    let retain_until_date_str: Option<String> = row.get(15).unwrap_or(None);
    let retain_until_date = retain_until_date_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .ok()
    });
    let legal_hold_status: Option<String> = row.get(16).unwrap_or(None);
    let storage_class: String = row.get(17).unwrap_or_else(|_| "STANDARD".to_string());
    let checksum_algorithm: Option<String> = row.get(18).unwrap_or(None);
    let checksum_value: Option<String> = row.get(19).unwrap_or(None);
    let replication_status: Option<String> = row.get(20).unwrap_or(None);
    let is_tombstone: bool = row.get::<_, i32>(21).unwrap_or(0) != 0;
    let lock_updated_at_str: Option<String> = row.get(22).unwrap_or(None);
    let lock_updated_at = lock_updated_at_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .ok()
    });

    Ok(ObjectRecord {
        bucket: row.get(0)?,
        key: row.get(1)?,
        blob_id: BlobId(row.get(2)?),
        size: row.get::<_, i64>(3)? as u64,
        etag: row.get(4)?,
        content_type: row.get(5)?,
        last_modified,
        metadata,
        encryption_algorithm,
        encryption_key_id,
        owner,
        version_id,
        is_latest,
        is_delete_marker,
        is_tombstone,
        retention_mode,
        retain_until_date,
        legal_hold_status,
        storage_class,
        checksum_algorithm,
        checksum_value,
        replication_status,
        lock_updated_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    fn make_record(bucket: &str, key: &str) -> ObjectRecord {
        ObjectRecord {
            bucket: bucket.to_string(),
            key: key.to_string(),
            blob_id: BlobId("test-blob-id".to_string()),
            size: 100,
            etag: "abc123".to_string(),
            content_type: Some("text/plain".to_string()),
            last_modified: chrono::Utc::now(),
            metadata: HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            replication_status: None,
            lock_updated_at: None,
        }
    }

    #[tokio::test]
    async fn list_referenced_blob_ids_excludes_tombstones_includes_parts() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();

        let mut r1 = make_record("b", "k1");
        r1.blob_id = BlobId("blob-1".to_string());
        let mut r2 = make_record("b", "k2");
        r2.blob_id = BlobId("blob-2".to_string());
        store.put_object(&r1).await.unwrap();
        store.put_object(&r2).await.unwrap();

        // An in-progress multipart part references its own blob (must NOT be GC'd).
        let mpu = MultipartUploadRecord {
            upload_id: "u1".to_string(),
            bucket: "b".to_string(),
            key: "big".to_string(),
            content_type: None,
            initiated_at: chrono::Utc::now(),
            metadata: HashMap::new(),
            checksum_algorithm: None,
        };
        store.create_multipart_upload(&mpu).await.unwrap();
        let part = PartRecord {
            upload_id: "u1".to_string(),
            part_number: 1,
            blob_id: BlobId("blob-part".to_string()),
            size: 5,
            etag: "e".to_string(),
            checksum_value: None,
            last_modified: None,
        };
        store.put_part(&part).await.unwrap();

        let refs: std::collections::HashSet<String> = store
            .list_referenced_blob_ids()
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.0)
            .collect();
        assert!(refs.contains("blob-1"));
        assert!(refs.contains("blob-2"));
        assert!(refs.contains("blob-part"));

        // Tombstoning k1 (cluster mode) drops blob-1 from the referenced set, so
        // GC may reclaim its bytes — but blob-2 and the in-progress part stay.
        store.set_cluster_mode(true);
        store.delete_object("b", "k1").await.unwrap();
        let refs2: std::collections::HashSet<String> = store
            .list_referenced_blob_ids()
            .await
            .unwrap()
            .into_iter()
            .map(|b| b.0)
            .collect();
        assert!(
            !refs2.contains("blob-1"),
            "a tombstoned object's blob is no longer referenced"
        );
        assert!(refs2.contains("blob-2"));
        assert!(refs2.contains("blob-part"));
    }

    // -- Stats tests --

    #[tokio::test]
    async fn get_stats_empty() {
        let store = test_store().await;
        let stats = store.get_stats().await.unwrap();
        assert_eq!(stats.bucket_count, 0);
        assert_eq!(stats.object_count, 0);
        assert_eq!(stats.total_size_bytes, 0);
    }

    #[tokio::test]
    async fn get_stats_with_data() {
        let store = test_store().await;
        store.create_bucket("a").await.unwrap();
        store.create_bucket("b").await.unwrap();

        let mut r1 = make_record("a", "key1");
        r1.size = 100;
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("a", "key2");
        r2.size = 250;
        r2.blob_id = BlobId("blob-2".to_string());
        store.put_object(&r2).await.unwrap();

        let stats = store.get_stats().await.unwrap();
        assert_eq!(stats.bucket_count, 2);
        assert_eq!(stats.object_count, 2);
        assert_eq!(stats.total_size_bytes, 350);
    }

    #[tokio::test]
    async fn get_stats_after_delete() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();

        let mut r1 = make_record("b", "key1");
        r1.size = 100;
        store.put_object(&r1).await.unwrap();

        store.delete_object("b", "key1").await.unwrap();

        let stats = store.get_stats().await.unwrap();
        assert_eq!(stats.bucket_count, 1);
        assert_eq!(stats.object_count, 0);
        assert_eq!(stats.total_size_bytes, 0);
    }

    // -- Bucket tests --

    #[tokio::test]
    async fn create_and_head_bucket() {
        let store = test_store().await;
        store.create_bucket("my-bucket").await.unwrap();

        let bucket = store
            .head_bucket("my-bucket")
            .await
            .unwrap()
            .expect("bucket should exist");

        assert_eq!(bucket.name, "my-bucket");
    }

    #[tokio::test]
    async fn apply_remote_bucket_upserts_and_is_idempotent() {
        let store = test_store().await;
        let info = BucketInfo {
            name: "replicated".to_string(),
            created_at: chrono::Utc::now(),
            owner: "alice".to_string(),
        };
        // Verbatim insert with no prior create_bucket.
        store.apply_remote_bucket(&info).await.unwrap();
        let got = store
            .head_bucket("replicated")
            .await
            .unwrap()
            .expect("bucket present");
        assert_eq!(got.name, "replicated");
        assert_eq!(got.owner, "alice");

        // Re-delivery with an updated owner overwrites in place, no error.
        let mut info2 = info.clone();
        info2.owner = "bob".to_string();
        store.apply_remote_bucket(&info2).await.unwrap();
        let got2 = store.head_bucket("replicated").await.unwrap().unwrap();
        assert_eq!(got2.owner, "bob");
    }

    #[tokio::test]
    async fn apply_remote_multipart_upload_inserts_and_is_idempotent() {
        let store = test_store().await;
        let record = MultipartUploadRecord {
            upload_id: "u-remote".to_string(),
            bucket: "b".to_string(),
            key: "k".to_string(),
            content_type: Some("text/plain".to_string()),
            initiated_at: chrono::Utc::now(),
            metadata: Default::default(),
            checksum_algorithm: None,
        };
        // Verbatim insert (no local create_multipart_upload first).
        store.apply_remote_multipart_upload(&record).await.unwrap();
        assert!(store.get_multipart_upload("u-remote").await.unwrap().is_some());
        // Re-delivery is a no-op (ON CONFLICT DO NOTHING), never an error.
        store.apply_remote_multipart_upload(&record).await.unwrap();
        assert!(store.get_multipart_upload("u-remote").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn create_duplicate_returns_error() {
        let store = test_store().await;
        store.create_bucket("my-bucket").await.unwrap();

        let result = store.create_bucket("my-bucket").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn head_nonexistent_returns_none() {
        let store = test_store().await;
        let result = store.head_bucket("no-such-bucket").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn list_empty() {
        let store = test_store().await;
        let buckets = store.list_buckets().await.unwrap();
        assert!(buckets.is_empty());
    }

    #[tokio::test]
    async fn list_ordered_by_name() {
        let store = test_store().await;
        store.create_bucket("charlie").await.unwrap();
        store.create_bucket("alpha").await.unwrap();
        store.create_bucket("bravo").await.unwrap();

        let buckets = store.list_buckets().await.unwrap();
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].name, "alpha");
        assert_eq!(buckets[1].name, "bravo");
        assert_eq!(buckets[2].name, "charlie");
    }

    #[tokio::test]
    async fn delete_existing_bucket() {
        let store = test_store().await;
        store.create_bucket("to-delete").await.unwrap();

        let deleted = store.delete_bucket("to-delete").await.unwrap();
        assert!(deleted);

        let result = store.head_bucket("to-delete").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let store = test_store().await;
        let deleted = store.delete_bucket("no-such-bucket").await.unwrap();
        assert!(!deleted);
    }

    // -- Object tests --

    #[tokio::test]
    async fn put_new_object() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let record = make_record("b", "key1");

        let (old, _vid) = store.put_object(&record).await.unwrap();
        assert!(old.is_none());

        let got = store.get_object("b", "key1").await.unwrap().unwrap();
        assert_eq!(got.key, "key1");
        assert_eq!(got.size, 100);
    }

    #[tokio::test]
    async fn put_overwrite_returns_old() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();

        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        r1.size = 100;
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        r2.size = 200;
        let (old, _vid) = store.put_object(&r2).await.unwrap();
        let old = old.unwrap();

        assert_eq!(old.blob_id.0, "blob-1");
        assert_eq!(old.size, 100);

        let got = store.get_object("b", "key1").await.unwrap().unwrap();
        assert_eq!(got.blob_id.0, "blob-2");
        assert_eq!(got.size, 200);
    }

    #[tokio::test]
    async fn get_nonexistent_object() {
        let store = test_store().await;
        let got = store.get_object("b", "nope").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn delete_object_returns_old() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let record = make_record("b", "key1");
        store.put_object(&record).await.unwrap();

        let old = store.delete_object("b", "key1").await.unwrap().unwrap();
        assert_eq!(old.key, "key1");

        let got = store.get_object("b", "key1").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn delete_nonexistent_object_returns_none() {
        let store = test_store().await;
        let old = store.delete_object("b", "nope").await.unwrap();
        assert!(old.is_none());
    }

    #[tokio::test]
    async fn bucket_is_empty_true() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        assert!(store.bucket_is_empty("b").await.unwrap());
    }

    #[tokio::test]
    async fn bucket_is_empty_false() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let record = make_record("b", "key1");
        store.put_object(&record).await.unwrap();
        assert!(!store.bucket_is_empty("b").await.unwrap());
    }

    // -- list_objects tests --

    #[tokio::test]
    async fn list_objects_empty_bucket() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let records = store.list_objects("b", None, None, 1000).await.unwrap();
        assert!(records.is_empty());
    }

    #[tokio::test]
    async fn list_objects_all() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        for key in ["c", "a", "b"] {
            let r = make_record("b", key);
            store.put_object(&r).await.unwrap();
        }
        let records = store.list_objects("b", None, None, 1000).await.unwrap();
        assert_eq!(records.len(), 3);
        // Should be sorted by key
        assert_eq!(records[0].key, "a");
        assert_eq!(records[1].key, "b");
        assert_eq!(records[2].key, "c");
    }

    #[tokio::test]
    async fn list_objects_prefix_filter() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        for key in ["photos/a.jpg", "photos/b.jpg", "videos/c.mp4"] {
            let r = make_record("b", key);
            store.put_object(&r).await.unwrap();
        }
        let records = store.list_objects("b", Some("photos/"), None, 1000).await.unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.key.starts_with("photos/")));
    }

    #[tokio::test]
    async fn list_objects_start_after() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        for key in ["a", "b", "c", "d"] {
            let r = make_record("b", key);
            store.put_object(&r).await.unwrap();
        }
        let records = store.list_objects("b", None, Some("b"), 1000).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].key, "c");
        assert_eq!(records[1].key, "d");
    }

    #[tokio::test]
    async fn list_objects_max_keys() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        for key in ["a", "b", "c", "d"] {
            let r = make_record("b", key);
            store.put_object(&r).await.unwrap();
        }
        let records = store.list_objects("b", None, None, 2).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].key, "a");
        assert_eq!(records[1].key, "b");
    }

    #[tokio::test]
    async fn list_objects_combined() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        for key in ["photos/a", "photos/b", "photos/c", "videos/d"] {
            let r = make_record("b", key);
            store.put_object(&r).await.unwrap();
        }
        let records = store
            .list_objects("b", Some("photos/"), Some("photos/a"), 1)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].key, "photos/b");
    }

    #[tokio::test]
    async fn list_objects_special_chars_in_prefix() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        // Keys with SQL LIKE special characters
        for key in ["100%_done", "100%_more", "other"] {
            let r = make_record("b", key);
            store.put_object(&r).await.unwrap();
        }
        let records = store.list_objects("b", Some("100%_"), None, 1000).await.unwrap();
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.key.starts_with("100%_")));
    }

    // -- Multipart upload tests --

    fn make_upload(upload_id: &str, bucket: &str, key: &str) -> MultipartUploadRecord {
        MultipartUploadRecord {
            upload_id: upload_id.to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            content_type: Some("application/octet-stream".to_string()),
            initiated_at: chrono::Utc::now(),
            metadata: HashMap::new(),
            checksum_algorithm: None,
        }
    }

    fn make_part(upload_id: &str, part_number: u32) -> PartRecord {
        PartRecord {
            upload_id: upload_id.to_string(),
            part_number,
            blob_id: BlobId(format!("blob-{upload_id}-{part_number}")),
            size: 5_242_880,
            etag: format!("etag-{part_number}"),
            checksum_value: None,
            last_modified: None,
        }
    }

    #[tokio::test]
    async fn create_and_get_multipart_upload() {
        let store = test_store().await;
        let upload = make_upload("up-1", "b", "key1");

        store.create_multipart_upload(&upload).await.unwrap();

        let got = store
            .get_multipart_upload("up-1")
            .await
            .unwrap()
            .expect("upload should exist");
        assert_eq!(got.upload_id, "up-1");
        assert_eq!(got.bucket, "b");
        assert_eq!(got.key, "key1");
        assert_eq!(got.content_type.as_deref(), Some("application/octet-stream"));
    }

    #[tokio::test]
    async fn get_nonexistent_upload_returns_none() {
        let store = test_store().await;
        let got = store.get_multipart_upload("no-such").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn put_part_new() {
        let store = test_store().await;
        let upload = make_upload("up-1", "b", "key1");
        store.create_multipart_upload(&upload).await.unwrap();

        let part = make_part("up-1", 1);
        let old = store.put_part(&part).await.unwrap();
        assert!(old.is_none());

        let parts = store.list_parts("up-1").await.unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].part_number, 1);
        assert_eq!(parts[0].blob_id.0, "blob-up-1-1");
    }

    #[tokio::test]
    async fn put_part_overwrite_returns_old() {
        let store = test_store().await;
        let upload = make_upload("up-1", "b", "key1");
        store.create_multipart_upload(&upload).await.unwrap();

        let part1 = make_part("up-1", 1);
        store.put_part(&part1).await.unwrap();

        let mut part1_v2 = make_part("up-1", 1);
        part1_v2.blob_id = BlobId("new-blob".to_string());
        let old = store.put_part(&part1_v2).await.unwrap().unwrap();
        assert_eq!(old.blob_id.0, "blob-up-1-1");

        let parts = store.list_parts("up-1").await.unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].blob_id.0, "new-blob");
    }

    #[tokio::test]
    async fn list_parts_ordered() {
        let store = test_store().await;
        let upload = make_upload("up-1", "b", "key1");
        store.create_multipart_upload(&upload).await.unwrap();

        // Insert in reverse order.
        for n in [3, 1, 2] {
            let part = make_part("up-1", n);
            store.put_part(&part).await.unwrap();
        }

        let parts = store.list_parts("up-1").await.unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].part_number, 1);
        assert_eq!(parts[1].part_number, 2);
        assert_eq!(parts[2].part_number, 3);
    }

    #[tokio::test]
    async fn list_parts_empty() {
        let store = test_store().await;
        let parts = store.list_parts("no-such").await.unwrap();
        assert!(parts.is_empty());
    }

    #[tokio::test]
    async fn delete_multipart_upload_returns_parts() {
        let store = test_store().await;
        let upload = make_upload("up-1", "b", "key1");
        store.create_multipart_upload(&upload).await.unwrap();

        let part1 = make_part("up-1", 1);
        let part2 = make_part("up-1", 2);
        store.put_part(&part1).await.unwrap();
        store.put_part(&part2).await.unwrap();

        let parts = store.delete_multipart_upload("up-1").await.unwrap();
        assert_eq!(parts.len(), 2);

        // Upload should be gone.
        let got = store.get_multipart_upload("up-1").await.unwrap();
        assert!(got.is_none());

        // Parts should be gone.
        let parts = store.list_parts("up-1").await.unwrap();
        assert!(parts.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_upload_returns_empty() {
        let store = test_store().await;
        let parts = store.delete_multipart_upload("no-such").await.unwrap();
        assert!(parts.is_empty());
    }

    // -- Versioning tests --

    /// Helper: enable versioning on a bucket.
    async fn enable_versioning(store: &SqliteStore, bucket: &str) {
        store
            .set_bucket_config(bucket, "versioning", "Enabled")
            .await
            .unwrap();
    }

    /// Helper: suspend versioning on a bucket.
    async fn suspend_versioning(store: &SqliteStore, bucket: &str) {
        store
            .set_bucket_config(bucket, "versioning", "Suspended")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn put_object_versioned_generates_version_id() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let r = make_record("b", "key1");
        let (old, _vid) = store.put_object(&r).await.unwrap();
        assert!(old.is_none()); // No blob to clean up

        let got = store.get_object("b", "key1").await.unwrap().unwrap();
        assert!(got.version_id.is_some());
        assert!(got.is_latest);
        assert!(!got.is_delete_marker);
    }

    #[tokio::test]
    async fn put_object_versioned_preserves_old_versions() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        r1.size = 100;
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        r2.size = 200;
        let (old, _vid) = store.put_object(&r2).await.unwrap();
        assert!(old.is_none()); // Old version preserved, no cleanup

        // Latest should be r2.
        let got = store.get_object("b", "key1").await.unwrap().unwrap();
        assert_eq!(got.blob_id.0, "blob-2");
        assert_eq!(got.size, 200);

        // list_object_versions should return both.
        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        assert_eq!(versions.len(), 2);
        // Newest first.
        assert_eq!(versions[0].blob_id.0, "blob-2");
        assert!(versions[0].is_latest);
        assert_eq!(versions[1].blob_id.0, "blob-1");
        assert!(!versions[1].is_latest);
    }

    #[tokio::test]
    async fn get_object_after_delete_marker_returns_none() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let r = make_record("b", "key1");
        store.put_object(&r).await.unwrap();

        // Delete creates a delete marker.
        let deleted = store.delete_object("b", "key1").await.unwrap();
        assert!(deleted.is_some());
        let dm = deleted.unwrap();
        assert!(dm.is_delete_marker);
        assert!(dm.version_id.is_some());

        // get_object should return None (latest is a delete marker).
        let got = store.get_object("b", "key1").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn get_object_version_specific() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        r1.size = 100;
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        r2.size = 200;
        store.put_object(&r2).await.unwrap();

        // Get the versions to find version IDs.
        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        let v1_id = versions[1].version_id.as_ref().unwrap();
        let v2_id = versions[0].version_id.as_ref().unwrap();

        // Fetch specific versions.
        let got_v1 = store
            .get_object_version("b", "key1", v1_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got_v1.blob_id.0, "blob-1");

        let got_v2 = store
            .get_object_version("b", "key1", v2_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got_v2.blob_id.0, "blob-2");
    }

    #[tokio::test]
    async fn delete_object_version_hard_deletes() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        store.put_object(&r2).await.unwrap();

        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        let v1_id = versions[1].version_id.as_ref().unwrap().clone();

        // Delete the old version permanently.
        let deleted = store
            .delete_object_version("b", "key1", &v1_id)
            .await
            .unwrap();
        assert!(deleted.is_some());
        assert_eq!(deleted.unwrap().blob_id.0, "blob-1");

        // Only one version should remain.
        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].blob_id.0, "blob-2");
    }

    #[tokio::test]
    async fn delete_object_version_promotes_next_latest() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        store.put_object(&r2).await.unwrap();

        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        let v2_id = versions[0].version_id.as_ref().unwrap().clone();

        // Delete the latest version permanently.
        store
            .delete_object_version("b", "key1", &v2_id)
            .await
            .unwrap();

        // The old version should now be latest.
        let got = store.get_object("b", "key1").await.unwrap().unwrap();
        assert_eq!(got.blob_id.0, "blob-1");
        assert!(got.is_latest);
    }

    #[tokio::test]
    async fn delete_delete_marker_undeletes_object() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let r = make_record("b", "key1");
        store.put_object(&r).await.unwrap();

        // Create a delete marker.
        let dm = store.delete_object("b", "key1").await.unwrap().unwrap();
        let dm_version_id = dm.version_id.unwrap();

        // Object should be invisible.
        assert!(store.get_object("b", "key1").await.unwrap().is_none());

        // Remove the delete marker.
        let deleted = store
            .delete_object_version("b", "key1", &dm_version_id)
            .await
            .unwrap();
        assert!(deleted.is_some());
        assert!(deleted.unwrap().is_delete_marker);

        // Object should be visible again.
        let got = store.get_object("b", "key1").await.unwrap();
        assert!(got.is_some());
    }

    #[tokio::test]
    async fn list_objects_only_latest_visible() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        // Create two versions of key1 and one of key2.
        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1a".to_string());
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-1b".to_string());
        store.put_object(&r2).await.unwrap();

        let r3 = make_record("b", "key2");
        store.put_object(&r3).await.unwrap();

        // list_objects should return only 2 objects (latest of each key).
        let records = store.list_objects("b", None, None, 100).await.unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].key, "key1");
        assert_eq!(records[0].blob_id.0, "blob-1b");
        assert_eq!(records[1].key, "key2");
    }

    #[tokio::test]
    async fn list_object_versions_includes_all() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        store.put_object(&r2).await.unwrap();

        // Delete to create a delete marker.
        store.delete_object("b", "key1").await.unwrap();

        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        assert_eq!(versions.len(), 3);
        // Most recent first: delete marker, blob-2, blob-1.
        assert!(versions[0].is_delete_marker);
        assert!(versions[0].is_latest);
        assert_eq!(versions[1].blob_id.0, "blob-2");
        assert!(!versions[1].is_latest);
        assert_eq!(versions[2].blob_id.0, "blob-1");
        assert!(!versions[2].is_latest);
    }

    #[tokio::test]
    async fn bucket_is_empty_with_delete_markers() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let r = make_record("b", "key1");
        store.put_object(&r).await.unwrap();

        // Delete the object (creates delete marker).
        store.delete_object("b", "key1").await.unwrap();

        // Bucket is NOT empty (still has versions + delete marker).
        assert!(!store.bucket_is_empty("b").await.unwrap());
    }

    #[tokio::test]
    async fn unversioned_bucket_unchanged_behavior() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();

        // No versioning enabled — should work exactly as before.
        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        r1.size = 100;
        let (old, _vid) = store.put_object(&r1).await.unwrap();
        assert!(old.is_none());

        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        r2.size = 200;
        let (old, _vid) = store.put_object(&r2).await.unwrap();
        // Should return old record for cleanup.
        assert!(old.is_some());
        assert_eq!(old.unwrap().blob_id.0, "blob-1");

        // Only one version in list_object_versions.
        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        assert_eq!(versions.len(), 1);

        // get_object returns the latest.
        let got = store.get_object("b", "key1").await.unwrap().unwrap();
        assert_eq!(got.blob_id.0, "blob-2");
        assert!(got.version_id.is_none());

        // delete_object hard-deletes.
        let old = store.delete_object("b", "key1").await.unwrap();
        assert!(old.is_some());
        assert_eq!(old.unwrap().blob_id.0, "blob-2");
        assert!(store.get_object("b", "key1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_object_suspended_overwrites_null_version() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();

        // First enable, put an object (gets a real version ID).
        enable_versioning(&store, "b").await;
        let mut r1 = make_record("b", "key1");
        r1.blob_id = BlobId("blob-1".to_string());
        store.put_object(&r1).await.unwrap();

        // Now suspend versioning.
        suspend_versioning(&store, "b").await;

        // Put again — should create a null version, keep the real version.
        let mut r2 = make_record("b", "key1");
        r2.blob_id = BlobId("blob-2".to_string());
        let (old, _vid) = store.put_object(&r2).await.unwrap();
        assert!(old.is_none()); // No previous null version to clean up

        // Put a third time — should overwrite the null version.
        let mut r3 = make_record("b", "key1");
        r3.blob_id = BlobId("blob-3".to_string());
        let (old, _vid) = store.put_object(&r3).await.unwrap();
        // Should return the old null version for cleanup.
        assert!(old.is_some());
        assert_eq!(old.unwrap().blob_id.0, "blob-2");

        // Total versions: real (blob-1) + null (blob-3) = 2
        let versions = store
            .list_object_versions("b", None, None, None, 100)
            .await
            .unwrap();
        assert_eq!(versions.len(), 2);
    }

    #[tokio::test]
    async fn stats_only_count_visible_objects() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;

        let mut r1 = make_record("b", "key1");
        r1.size = 100;
        store.put_object(&r1).await.unwrap();

        let mut r2 = make_record("b", "key1");
        r2.size = 200;
        r2.blob_id = BlobId("blob-2".to_string());
        store.put_object(&r2).await.unwrap();

        let stats = store.get_stats().await.unwrap();
        // Only the latest version counts.
        assert_eq!(stats.object_count, 1);
        assert_eq!(stats.total_size_bytes, 200);
    }

    // ----- Cluster replication: apply_remote_* (Phase 29 HA) -----

    #[tokio::test]
    async fn apply_remote_object_inserts_and_is_visible() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let r = make_record("b", "k"); // null-version
        store.apply_remote_object(&r).await.unwrap();
        let got = store.get_object("b", "k").await.unwrap();
        assert!(got.is_some());
        assert!(got.unwrap().is_latest);
    }

    #[tokio::test]
    async fn apply_remote_object_idempotent() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let mut r = make_record("b", "k");
        r.version_id = Some("v1".to_string());
        store.apply_remote_object(&r).await.unwrap();
        store.apply_remote_object(&r).await.unwrap();
        let versions = store.list_object_versions("b", None, None, None, 100).await.unwrap();
        assert_eq!(versions.len(), 1, "re-delivery must not duplicate the version");
    }

    #[tokio::test]
    async fn apply_remote_lww_null_version_newest_wins() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let t1 = chrono::Utc::now();
        let t2 = t1 + chrono::Duration::seconds(10);

        let mut older = make_record("b", "k");
        older.version_id = None;
        older.blob_id = BlobId("blob-old".to_string());
        older.last_modified = t1;
        older.etag = "old".to_string();

        let mut newer = make_record("b", "k");
        newer.version_id = None;
        newer.blob_id = BlobId("blob-new".to_string());
        newer.last_modified = t2;
        newer.etag = "new".to_string();

        // Apply out of order (newer first): LWW must still pick the newer one,
        // and there must be exactly one null-version row.
        store.apply_remote_object(&newer).await.unwrap();
        store.apply_remote_object(&older).await.unwrap();

        let got = store.get_object("b", "k").await.unwrap().unwrap();
        assert_eq!(got.etag, "new");
        assert!(got.is_latest);
        let versions = store.list_object_versions("b", None, None, None, 100).await.unwrap();
        assert_eq!(versions.len(), 1, "unversioned key must converge to one row");
    }

    #[tokio::test]
    async fn apply_remote_versioned_recomputes_latest_regardless_of_order() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let t1 = chrono::Utc::now();
        let t2 = t1 + chrono::Duration::seconds(10);

        let mut v1 = make_record("b", "k");
        v1.version_id = Some("v1".to_string());
        v1.blob_id = BlobId("blob-1".to_string());
        v1.last_modified = t1;

        let mut v2 = make_record("b", "k");
        v2.version_id = Some("v2".to_string());
        v2.blob_id = BlobId("blob-2".to_string());
        v2.last_modified = t2;

        // Apply newest first, then oldest: recompute is deterministic on
        // (last_modified, version_id, blob_id), so v2 is latest either way.
        store.apply_remote_object(&v2).await.unwrap();
        store.apply_remote_object(&v1).await.unwrap();

        let versions = store.list_object_versions("b", None, None, None, 100).await.unwrap();
        assert_eq!(versions.len(), 2, "both versions retained");
        assert!(store.get_object_version("b", "k", "v2").await.unwrap().unwrap().is_latest);
        assert!(!store.get_object_version("b", "k", "v1").await.unwrap().unwrap().is_latest);
    }

    #[tokio::test]
    async fn apply_remote_version_delete_promotes_next_latest() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let t1 = chrono::Utc::now();
        let t2 = t1 + chrono::Duration::seconds(10);

        let mut v1 = make_record("b", "k");
        v1.version_id = Some("v1".to_string());
        v1.blob_id = BlobId("blob-1".to_string());
        v1.last_modified = t1;
        let mut v2 = make_record("b", "k");
        v2.version_id = Some("v2".to_string());
        v2.blob_id = BlobId("blob-2".to_string());
        v2.last_modified = t2;

        store.apply_remote_object(&v1).await.unwrap();
        store.apply_remote_object(&v2).await.unwrap();
        // Delete the current latest (v2): v1 must be promoted.
        store.apply_remote_version_delete("b", "k", "v2").await.unwrap();

        let versions = store.list_object_versions("b", None, None, None, 100).await.unwrap();
        assert_eq!(versions.len(), 1);
        assert!(store.get_object_version("b", "k", "v1").await.unwrap().unwrap().is_latest);
    }

    #[tokio::test]
    async fn changed_since_orders_by_seq_and_advances_cursor() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        for k in ["k1", "k2", "k3"] {
            store.put_object(&make_record("b", k)).await.unwrap();
        }

        // From 0: all three, strictly increasing seq (the changed-since cursor).
        let all = store.list_rows_changed_since(0, 100).await.unwrap();
        assert_eq!(all.len(), 3);
        let seqs: Vec<u64> = all.iter().map(|(s, _)| *s).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seq must ascend: {seqs:?}");

        // A peer that already saw the first row asks for everything newer.
        let after_first = store.list_rows_changed_since(seqs[0], 100).await.unwrap();
        assert_eq!(after_first.len(), 2);
        assert_eq!(after_first[0].0, seqs[1]);

        // Overwriting k1 (unversioned) stamps a fresh, higher seq so the rewrite
        // is re-pulled even though the peer already had the old k1.
        store.put_object(&make_record("b", "k1")).await.unwrap();
        let after_all = store.list_rows_changed_since(seqs[2], 100).await.unwrap();
        assert_eq!(after_all.len(), 1, "only the rewritten k1 is newer");
        assert_eq!(after_all[0].1.key, "k1");
        assert!(after_all[0].0 > seqs[2]);
    }

    #[tokio::test]
    async fn changed_since_respects_limit() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        for k in ["k1", "k2", "k3"] {
            store.put_object(&make_record("b", k)).await.unwrap();
        }
        let page = store.list_rows_changed_since(0, 2).await.unwrap();
        assert_eq!(page.len(), 2, "limit caps the batch");
    }

    #[tokio::test]
    async fn current_object_seq_tracks_the_counter() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        assert_eq!(store.current_object_seq().await.unwrap(), 0, "fresh store");

        store.put_object(&make_record("b", "k1")).await.unwrap();
        store.put_object(&make_record("b", "k2")).await.unwrap();
        let after_writes = store.current_object_seq().await.unwrap();
        let max_listed = store
            .list_rows_changed_since(0, 100)
            .await
            .unwrap()
            .iter()
            .map(|(s, _)| *s)
            .max()
            .unwrap();
        assert_eq!(
            after_writes, max_listed,
            "counter equals the highest assigned seq"
        );
        // Reading must not advance it.
        assert_eq!(store.current_object_seq().await.unwrap(), after_writes);
    }

    #[tokio::test]
    async fn lock_changes_stamp_a_fresh_seq() {
        // N1: retention/legal-hold UPDATEs must advance the row's seq so a peer
        // that was down during the lock change pulls it via changed-since
        // (real-time fan-out is otherwise the only path that carries it).
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        store.put_object(&make_record("b", "k")).await.unwrap();
        let baseline = store.current_object_seq().await.unwrap();

        assert!(store
            .set_object_retention("b", "k", None, Some("GOVERNANCE"), Some("2030-01-01T00:00:00Z"))
            .await
            .unwrap());
        let after_retention = store.list_rows_changed_since(baseline, 100).await.unwrap();
        assert_eq!(after_retention.len(), 1, "retention change must be manifest-visible");
        assert_eq!(after_retention[0].1.retention_mode.as_deref(), Some("GOVERNANCE"));

        let mid = store.current_object_seq().await.unwrap();
        assert!(store.set_object_legal_hold("b", "k", None, Some("ON")).await.unwrap());
        let after_hold = store.list_rows_changed_since(mid, 100).await.unwrap();
        assert_eq!(after_hold.len(), 1, "legal-hold change must be manifest-visible");
        assert_eq!(after_hold[0].1.legal_hold_status.as_deref(), Some("ON"));
    }

    #[tokio::test]
    async fn lock_changes_stamp_a_fresh_seq_on_versioned_rows() {
        // Same N1 guarantee for the explicit version_id branch. Seeded via
        // apply_remote_object (verbatim insert): put_object generates its own
        // version ids.
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let mut rec = make_record("b", "k");
        rec.version_id = Some("v1".to_string());
        store.apply_remote_object(&rec).await.unwrap();
        let baseline = store.current_object_seq().await.unwrap();

        assert!(store
            .set_object_retention("b", "k", Some("v1"), Some("COMPLIANCE"), Some("2031-01-01T00:00:00Z"))
            .await
            .unwrap());
        assert!(store.set_object_legal_hold("b", "k", Some("v1"), Some("OFF")).await.unwrap());
        let changed = store.list_rows_changed_since(baseline, 100).await.unwrap();
        // Two lock updates on the same row → the row appears once, at the
        // newest seq, carrying both columns.
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].1.retention_mode.as_deref(), Some("COMPLIANCE"));
        assert_eq!(changed[0].1.legal_hold_status.as_deref(), Some("OFF"));
    }

    #[tokio::test]
    async fn changed_since_includes_remote_applied_rows() {
        // apply_remote_object must also stamp seq, so a row this node learned
        // from a peer is itself pulled by a third node's changed-since scan
        // (transitive reconciliation A→B→C).
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        store.put_object(&make_record("b", "local")).await.unwrap();

        let mut remote = make_record("b", "remote");
        remote.version_id = Some("v-remote".to_string());
        store.apply_remote_object(&remote).await.unwrap();

        let all = store.list_rows_changed_since(0, 100).await.unwrap();
        let keys: Vec<&str> = all.iter().map(|(_, r)| r.key.as_str()).collect();
        assert!(keys.contains(&"remote"), "remote-applied row must carry a seq: {keys:?}");
        // The remote row's seq is the highest (applied last).
        let remote_seq = all.iter().find(|(_, r)| r.key == "remote").unwrap().0;
        let local_seq = all.iter().find(|(_, r)| r.key == "local").unwrap().0;
        assert!(remote_seq > local_seq);
    }

    #[tokio::test]
    async fn apply_remote_object_noops_on_identical_rows() {
        // M7: re-applying a row a node already holds must not rewrite it nor
        // stamp a fresh seq, otherwise two caught-up nodes redeliver their
        // whole object tables to each other on every anti-entropy pass.
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();

        let mut versioned = make_record("b", "k");
        versioned.version_id = Some("v1".to_string());
        store.apply_remote_object(&versioned).await.unwrap();
        let after_first = store.current_object_seq().await.unwrap();
        store.apply_remote_object(&versioned).await.unwrap();
        assert_eq!(
            store.current_object_seq().await.unwrap(),
            after_first,
            "identical versioned re-apply must be a no-op"
        );

        let null_version = make_record("b", "nv");
        store.apply_remote_object(&null_version).await.unwrap();
        let after_nv = store.current_object_seq().await.unwrap();
        store.apply_remote_object(&null_version).await.unwrap();
        assert_eq!(
            store.current_object_seq().await.unwrap(),
            after_nv,
            "identical null-version re-apply must be a no-op"
        );
    }

    #[tokio::test]
    async fn apply_remote_object_still_applies_lock_only_changes() {
        // N1+N2+M7 interplay: a row equal on the LWW tuple (last_modified,
        // blob_id) but with a NEWER lock state (lock_updated_at) must still be
        // applied — the equal-timestamp lock tiebreak exists for exactly this
        // case (set_object_retention stamps lock_updated_at, so the fanned-out
        // / manifest row always carries it).
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let mut rec = make_record("b", "k");
        rec.version_id = Some("v1".to_string());
        store.apply_remote_object(&rec).await.unwrap();
        let seq_before = store.current_object_seq().await.unwrap();

        let mut locked = rec.clone();
        locked.retention_mode = Some("GOVERNANCE".to_string());
        locked.retain_until_date =
            Some(chrono::DateTime::parse_from_rfc3339("2031-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc));
        locked.lock_updated_at = Some(chrono::Utc::now());
        store.apply_remote_object(&locked).await.unwrap();
        assert!(
            store.current_object_seq().await.unwrap() > seq_before,
            "lock-only change must be applied and re-stamped"
        );
        let got = store.get_object_version("b", "k", "v1").await.unwrap().unwrap();
        assert_eq!(got.retention_mode.as_deref(), Some("GOVERNANCE"));
    }

    #[tokio::test]
    async fn apply_remote_object_stale_copy_cannot_clobber_newer_lock_state() {
        // N2 regression (the phase-D retention clobber): after a lock change,
        // re-applying the STALE pre-lock copy of the same version (same
        // last_modified, no lock columns, no lock_updated_at) — exactly what a
        // restarted peer redelivers from its full manifest — must be a no-op,
        // not a clobber. The old `>=` guard let it through in both directions.
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let mut stale = make_record("b", "k");
        stale.version_id = Some("v1".to_string());
        store.apply_remote_object(&stale).await.unwrap();

        // The local lock change (stamps seq + lock_updated_at).
        assert!(store
            .set_object_retention("b", "k", None, Some("GOVERNANCE"), Some("2031-01-01T00:00:00Z"))
            .await
            .unwrap());
        let seq_after_lock = store.current_object_seq().await.unwrap();

        // The stale copy comes back (e.g. pulled from a peer that never
        // learned the lock change): it must NOT win.
        store.apply_remote_object(&stale).await.unwrap();
        let got = store.get_object_version("b", "k", "v1").await.unwrap().unwrap();
        assert_eq!(
            got.retention_mode.as_deref(),
            Some("GOVERNANCE"),
            "a stale equal-timestamp copy must not clobber a newer lock state"
        );
        assert_eq!(
            store.current_object_seq().await.unwrap(),
            seq_after_lock,
            "the rejected stale copy must not stamp a fresh seq (no churn, no propagation)"
        );
    }

    #[tokio::test]
    async fn apply_remote_object_stale_copy_cannot_clobber_lock_on_null_version() {
        // N2, null-version branch: same regression for unversioned objects —
        // the (last_modified, blob_id) register ties, and lock_updated_at must
        // break the tie instead of the old blob_id `>=` pass-through.
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let stale = make_record("b", "k"); // version_id = None
        store.apply_remote_object(&stale).await.unwrap();

        assert!(store
            .set_object_legal_hold("b", "k", None, Some("ON"))
            .await
            .unwrap());

        store.apply_remote_object(&stale).await.unwrap();
        let got = store.get_object("b", "k").await.unwrap().unwrap();
        assert_eq!(
            got.legal_hold_status.as_deref(),
            Some("ON"),
            "a stale equal-register copy must not clobber a newer legal hold"
        );
    }

    #[tokio::test]
    async fn lock_updates_stamp_lock_updated_at() {
        // N2: both lock mutations must record WHEN the lock state changed —
        // the LWW dimension the apply guards order ties by.
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        store.put_object(&make_record("b", "k")).await.unwrap();
        assert!(store.get_object("b", "k").await.unwrap().unwrap().lock_updated_at.is_none());

        store
            .set_object_retention("b", "k", None, Some("GOVERNANCE"), Some("2031-01-01T00:00:00Z"))
            .await
            .unwrap();
        let after_retention = store.get_object("b", "k").await.unwrap().unwrap().lock_updated_at;
        assert!(after_retention.is_some());

        store.set_object_legal_hold("b", "k", None, Some("ON")).await.unwrap();
        let after_hold = store.get_object("b", "k").await.unwrap().unwrap().lock_updated_at;
        assert!(after_hold > after_retention, "each lock change refreshes the stamp");
    }

    /// A store with cluster mode on, so hard deletes tombstone instead of remove.
    async fn cluster_store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store.set_cluster_mode(true);
        store
    }

    #[tokio::test]
    async fn cluster_hard_delete_leaves_tombstone_invisible_to_reads() {
        let store = cluster_store().await;
        store.create_bucket("b").await.unwrap();
        store.put_object(&make_record("b", "k")).await.unwrap();
        store.delete_object("b", "k").await.unwrap();

        // Invisible to S3 reads (the object is deleted)...
        assert!(store.get_object("b", "k").await.unwrap().is_none());
        assert!(store.get_latest_object("b", "k").await.unwrap().is_none());
        assert!(store.list_objects("b", None, None, 100).await.unwrap().is_empty());

        // ...but present as a tombstone in the changed-since manifest, so the
        // deletion propagates to peers and is not resurrected by anti-entropy.
        let rows = store.list_rows_changed_since(0, 100).await.unwrap();
        let tomb = rows.iter().find(|(_, r)| r.key == "k").expect("tombstone in manifest");
        assert!(tomb.1.is_tombstone);
        assert_eq!(tomb.1.blob_id.0, "", "tombstone carries no blob");
    }

    #[tokio::test]
    async fn single_node_hard_delete_removes_row_without_tombstone() {
        let store = test_store().await; // cluster mode off
        store.create_bucket("b").await.unwrap();
        store.put_object(&make_record("b", "k")).await.unwrap();
        store.delete_object("b", "k").await.unwrap();

        assert!(store.get_object("b", "k").await.unwrap().is_none());
        // Single node removes the row outright — no tombstone overhead.
        let rows = store.list_rows_changed_since(0, 100).await.unwrap();
        assert!(rows.iter().all(|(_, r)| !r.is_tombstone), "single node must not tombstone");
        assert!(!rows.iter().any(|(_, r)| r.key == "k"), "row removed");
    }

    #[tokio::test]
    async fn tombstone_blocks_anti_entropy_resurrection() {
        // Delete a version (tombstone), then a peer that missed the delete ships
        // the still-live version row via anti-entropy. It must NOT resurrect.
        let store = cluster_store().await;
        store.create_bucket("b").await.unwrap();
        enable_versioning(&store, "b").await;
        let (_, vid) = store.put_object(&make_record("b", "k")).await.unwrap();
        let vid = vid.unwrap();

        store.delete_object_version("b", "k", &vid).await.unwrap();
        assert!(store.get_object_version("b", "k", &vid).await.unwrap().is_none());

        // Stale live copy of the same version (its original, older last_modified).
        let mut stale = make_record("b", "k");
        stale.version_id = Some(vid.clone());
        stale.last_modified = chrono::Utc::now() - chrono::Duration::seconds(60);
        store.apply_remote_object(&stale).await.unwrap();

        assert!(
            store.get_object_version("b", "k", &vid).await.unwrap().is_none(),
            "a stale live row must not resurrect a tombstoned version"
        );
    }

    #[tokio::test]
    async fn purge_tombstones_deletes_by_age() {
        let store = cluster_store().await;
        store.create_bucket("b").await.unwrap();
        store.put_object(&make_record("b", "k1")).await.unwrap();
        store.put_object(&make_record("b", "k2")).await.unwrap();
        store.delete_object("b", "k1").await.unwrap();
        store.delete_object("b", "k2").await.unwrap();

        // A past cutoff keeps the just-created tombstones; a future cutoff (past
        // their grace) removes them.
        assert_eq!(
            store.purge_tombstones(chrono::Utc::now() - chrono::Duration::days(1)).await.unwrap(),
            0
        );
        assert_eq!(
            store.purge_tombstones(chrono::Utc::now() + chrono::Duration::seconds(1)).await.unwrap(),
            2
        );
    }

    // -- Re-encryption (Phase 30 M0) --

    #[tokio::test]
    async fn update_object_encryption_sets_and_clears_columns() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        store.put_object(&make_record("b", "k")).await.unwrap();
        let seq_before = store.current_object_seq().await.unwrap();

        // Set encryption metadata in place (same blob_id).
        assert!(store
            .update_object_encryption("b", "k", None, Some("AES256"), Some("deadbeef"))
            .await
            .unwrap());
        let got = store.get_object("b", "k").await.unwrap().unwrap();
        assert_eq!(got.encryption_algorithm.as_deref(), Some("AES256"));
        assert_eq!(got.encryption_key_id.as_deref(), Some("deadbeef"));
        assert_eq!(got.blob_id.0, "test-blob-id", "blob_id is unchanged in place");
        // The change bumped the node-local seq so it travels via the manifest.
        assert!(store.current_object_seq().await.unwrap() > seq_before);

        // Clearing (after a decrypt) drops both columns.
        assert!(store
            .update_object_encryption("b", "k", None, None, None)
            .await
            .unwrap());
        let got = store.get_object("b", "k").await.unwrap().unwrap();
        assert!(got.encryption_algorithm.is_none());
        assert!(got.encryption_key_id.is_none());

        // Missing object → no row updated.
        assert!(!store
            .update_object_encryption("b", "missing", None, Some("AES256"), None)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn update_object_encryption_cas_swaps_only_on_matching_blob_id() {
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let mut r = make_record("b", "k");
        r.blob_id = BlobId("old-blob".to_string());
        store.put_object(&r).await.unwrap();

        // CAS with the wrong old blob_id does nothing (a concurrent overwrite won).
        assert!(!store
            .update_object_encryption_cas(
                "b",
                "k",
                None,
                &BlobId("stale".to_string()),
                &BlobId("new-blob".to_string()),
                Some("AES256"),
                Some("deadbeef"),
            )
            .await
            .unwrap());
        let got = store.get_object("b", "k").await.unwrap().unwrap();
        assert_eq!(got.blob_id.0, "old-blob");
        assert!(got.encryption_algorithm.is_none());

        // CAS with the matching old blob_id swaps the blob and sets encryption.
        assert!(store
            .update_object_encryption_cas(
                "b",
                "k",
                None,
                &BlobId("old-blob".to_string()),
                &BlobId("new-blob".to_string()),
                Some("AES256"),
                Some("deadbeef"),
            )
            .await
            .unwrap());
        let got = store.get_object("b", "k").await.unwrap().unwrap();
        assert_eq!(got.blob_id.0, "new-blob");
        assert_eq!(got.encryption_algorithm.as_deref(), Some("AES256"));
        assert_eq!(got.encryption_key_id.as_deref(), Some("deadbeef"));
    }

    #[tokio::test]
    async fn apply_remote_reencrypted_row_converges_via_lock_updated_at() {
        // A re-encrypted row keeps the same last_modified but changes blob_id +
        // encryption columns and stamps a fresh lock_updated_at. It MUST be
        // adopted by a peer (cluster convergence), and a stale plain copy with
        // the same last_modified must NOT clobber it back.
        let store = test_store().await;
        store.create_bucket("b").await.unwrap();
        let mut orig = make_record("b", "k");
        orig.blob_id = BlobId("old-blob".to_string());
        store.put_object(&orig).await.unwrap();
        let stored = store.get_object("b", "k").await.unwrap().unwrap();
        assert!(stored.encryption_algorithm.is_none());

        let mut reenc = stored.clone();
        reenc.blob_id = BlobId("new-blob".to_string());
        reenc.encryption_algorithm = Some("AES256".to_string());
        reenc.encryption_key_id = Some("deadbeef".to_string());
        reenc.lock_updated_at = Some(stored.last_modified + chrono::Duration::seconds(5));

        store.apply_remote_object(&reenc).await.unwrap();
        let after = store.get_object("b", "k").await.unwrap().unwrap();
        assert_eq!(after.blob_id.0, "new-blob");
        assert_eq!(after.encryption_algorithm.as_deref(), Some("AES256"));

        // Re-applying the stale plain copy (lock_updated_at None, same
        // last_modified) must not revert the re-encryption.
        store.apply_remote_object(&stored).await.unwrap();
        let after2 = store.get_object("b", "k").await.unwrap().unwrap();
        assert_eq!(
            after2.blob_id.0, "new-blob",
            "stale plain copy must not clobber the re-encryption"
        );
        assert_eq!(after2.encryption_algorithm.as_deref(), Some("AES256"));
    }
}
