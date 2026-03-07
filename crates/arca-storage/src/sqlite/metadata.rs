//! `MetadataStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::MetadataStore;
use arca_core::types::{
    BlobId, BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats,
};
use std::collections::HashMap;
use chrono::DateTime;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl MetadataStore for SqliteStore {
    async fn get_stats(&self) -> Result<StorageStats, ArcaError> {
        self.conn
            .call(move |conn| {
                let stats = conn.query_row(
                    "SELECT
                        (SELECT COUNT(*) FROM buckets),
                        (SELECT COUNT(*) FROM objects),
                        (SELECT COALESCE(SUM(size), 0) FROM objects)",
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
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT name, created_at FROM buckets ORDER BY name",
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
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT name, created_at FROM buckets WHERE name = ?1",
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
                let affected =
                    conn.execute("DELETE FROM buckets WHERE name = ?1", params![name])?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_bucket: {e}")))
    }

    async fn bucket_is_empty(&self, name: &str) -> Result<bool, ArcaError> {
        let name = name.to_string();
        self.conn
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
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let record = record.clone();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;

                // Check for existing object to return for cleanup.
                let old = {
                    let mut stmt = tx.prepare(
                        "SELECT bucket, key, blob_id, size, etag, content_type, last_modified, metadata
                         FROM objects WHERE bucket = ?1 AND key = ?2",
                    )?;
                    let result = stmt.query_row(
                        params![record.bucket, record.key],
                        |row| Ok(row_to_object_record(row)),
                    );
                    match result {
                        Ok(rec) => Some(rec?),
                        Err(rusqlite::Error::QueryReturnedNoRows) => None,
                        Err(e) => return Err(e.into()),
                    }
                };

                let metadata_json = serde_json::to_string(&record.metadata)
                    .unwrap_or_else(|_| "{}".to_string());

                // Delete old record if exists, then insert new.
                tx.execute(
                    "DELETE FROM objects WHERE bucket = ?1 AND key = ?2",
                    params![record.bucket, record.key],
                )?;
                tx.execute(
                    "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        record.bucket,
                        record.key,
                        record.blob_id.0,
                        record.size as i64,
                        record.etag,
                        record.content_type,
                        record.last_modified.to_rfc3339(),
                        metadata_json,
                    ],
                )?;

                tx.commit()?;
                Ok(old)
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
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT bucket, key, blob_id, size, etag, content_type, last_modified, metadata
                     FROM objects WHERE bucket = ?1 AND key = ?2",
                )?;
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
        self.conn
            .call(move |conn| {
                // Build dynamic SQL.
                let mut sql = String::from(
                    "SELECT bucket, key, blob_id, size, etag, content_type, last_modified, metadata
                     FROM objects WHERE bucket = ?1",
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
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;

                let old = {
                    let mut stmt = tx.prepare(
                        "SELECT bucket, key, blob_id, size, etag, content_type, last_modified, metadata
                         FROM objects WHERE bucket = ?1 AND key = ?2",
                    )?;
                    let result = stmt.query_row(
                        params![bucket, key],
                        |row| Ok(row_to_object_record(row)),
                    );
                    match result {
                        Ok(rec) => Some(rec?),
                        Err(rusqlite::Error::QueryReturnedNoRows) => None,
                        Err(e) => return Err(e.into()),
                    }
                };

                if old.is_some() {
                    tx.execute(
                        "DELETE FROM objects WHERE bucket = ?1 AND key = ?2",
                        params![bucket, key],
                    )?;
                }

                tx.commit()?;
                Ok(old)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_object: {e}")))
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
                    "INSERT INTO multipart_uploads (upload_id, bucket, key, content_type, initiated_at, metadata)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        record.upload_id,
                        record.bucket,
                        record.key,
                        record.content_type,
                        record.initiated_at.to_rfc3339(),
                        metadata_json,
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
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT upload_id, bucket, key, content_type, initiated_at, metadata
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
                        "SELECT upload_id, part_number, blob_id, size, etag
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
                tx.execute(
                    "INSERT INTO parts (upload_id, part_number, blob_id, size, etag)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        part.upload_id,
                        part.part_number,
                        part.blob_id.0,
                        part.size as i64,
                        part.etag,
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
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT upload_id, part_number, blob_id, size, etag
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
                        "SELECT upload_id, part_number, blob_id, size, etag
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
        self.conn
            .call(move |conn| {
                let mut sql = String::from(
                    "SELECT upload_id, bucket, key, content_type, initiated_at, metadata
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
/// Expects columns: name, created_at.
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
    })
}

/// Converts a SQLite row to a `MultipartUploadRecord`.
///
/// Expects columns: upload_id, bucket, key, content_type, initiated_at, metadata.
fn row_to_multipart_upload_record(
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

    Ok(MultipartUploadRecord {
        upload_id: row.get(0)?,
        bucket: row.get(1)?,
        key: row.get(2)?,
        content_type: row.get(3)?,
        initiated_at,
        metadata,
    })
}

/// Converts a SQLite row to a `PartRecord`.
///
/// Expects columns: upload_id, part_number, blob_id, size, etag.
fn row_to_part_record(row: &rusqlite::Row) -> Result<PartRecord, rusqlite::Error> {
    Ok(PartRecord {
        upload_id: row.get(0)?,
        part_number: row.get(1)?,
        blob_id: BlobId(row.get(2)?),
        size: row.get::<_, i64>(3)? as u64,
        etag: row.get(4)?,
    })
}

/// Converts a SQLite row to an `ObjectRecord`.
///
/// Expects columns: bucket, key, blob_id, size, etag, content_type, last_modified, metadata.
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

    Ok(ObjectRecord {
        bucket: row.get(0)?,
        key: row.get(1)?,
        blob_id: BlobId(row.get(2)?),
        size: row.get::<_, i64>(3)? as u64,
        etag: row.get(4)?,
        content_type: row.get(5)?,
        last_modified,
        metadata,
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
        }
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

        let old = store.put_object(&record).await.unwrap();
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
        let old = store.put_object(&r2).await.unwrap().unwrap();

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
        }
    }

    fn make_part(upload_id: &str, part_number: u32) -> PartRecord {
        PartRecord {
            upload_id: upload_id.to_string(),
            part_number,
            blob_id: BlobId(format!("blob-{upload_id}-{part_number}")),
            size: 5_242_880,
            etag: format!("etag-{part_number}"),
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
}
