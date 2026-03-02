//! `MetadataStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::MetadataStore;
use arca_core::types::{BlobId, BucketInfo, ObjectRecord};
use chrono::DateTime;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl MetadataStore for SqliteStore {
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
                        "SELECT bucket, key, blob_id, size, etag, content_type, last_modified
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

                // Delete old record if exists, then insert new.
                tx.execute(
                    "DELETE FROM objects WHERE bucket = ?1 AND key = ?2",
                    params![record.bucket, record.key],
                )?;
                tx.execute(
                    "INSERT INTO objects (bucket, key, blob_id, size, etag, content_type, last_modified)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        record.bucket,
                        record.key,
                        record.blob_id.0,
                        record.size as i64,
                        record.etag,
                        record.content_type,
                        record.last_modified.to_rfc3339(),
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
                    "SELECT bucket, key, blob_id, size, etag, content_type, last_modified
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
                    "SELECT bucket, key, blob_id, size, etag, content_type, last_modified
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
                        "SELECT bucket, key, blob_id, size, etag, content_type, last_modified
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

/// Converts a SQLite row to an `ObjectRecord`.
///
/// Expects columns: bucket, key, blob_id, size, etag, content_type, last_modified.
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

    Ok(ObjectRecord {
        bucket: row.get(0)?,
        key: row.get(1)?,
        blob_id: BlobId(row.get(2)?),
        size: row.get::<_, i64>(3)? as u64,
        etag: row.get(4)?,
        content_type: row.get(5)?,
        last_modified,
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
        }
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
}
