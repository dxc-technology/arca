//! SQLite implementation of the `PresignedUrlStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::presigned_url::{PresignedUrlRecord, PresignedUrlStore};
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

fn row_to_presigned_url(row: &rusqlite::Row<'_>) -> rusqlite::Result<PresignedUrlRecord> {
    Ok(PresignedUrlRecord {
        id: row.get("id")?,
        bucket: row.get("bucket")?,
        key: row.get("key")?,
        method: row.get("method")?,
        expires_seconds: row.get::<_, i64>("expires_seconds")? as u64,
        created_at: row
            .get::<_, String>("created_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        expires_at: row
            .get::<_, String>("expires_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        access_key_id: row.get("access_key_id")?,
    })
}

#[async_trait::async_trait]
impl PresignedUrlStore for SqliteStore {
    async fn insert_presigned_url(
        &self,
        record: &PresignedUrlRecord,
    ) -> Result<(), ArcaError> {
        let record = record.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO presigned_urls
                        (id, bucket, key, method, expires_seconds,
                         created_at, expires_at, access_key_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        record.id,
                        record.bucket,
                        record.key,
                        record.method,
                        record.expires_seconds as i64,
                        record.created_at.to_rfc3339(),
                        record.expires_at.to_rfc3339(),
                        record.access_key_id,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("insert_presigned_url: {e}"))
            })
    }

    async fn list_presigned_urls(
        &self,
        bucket: &str,
    ) -> Result<Vec<PresignedUrlRecord>, ArcaError> {
        let bucket = bucket.to_string();
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT * FROM presigned_urls
                     WHERE bucket = ?1 AND expires_at > ?2
                     ORDER BY created_at DESC",
                )?;
                let rows = stmt.query_map(params![bucket, now], row_to_presigned_url)?;
                let mut entries = Vec::new();
                for row in rows {
                    entries.push(row?);
                }
                Ok(entries)
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("list_presigned_urls: {e}"))
            })
    }

    async fn delete_presigned_url(
        &self,
        id: &str,
    ) -> Result<bool, ArcaError> {
        let id = id.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM presigned_urls WHERE id = ?1",
                    params![id],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("delete_presigned_url: {e}"))
            })
    }

    async fn purge_expired_presigned_urls(
        &self,
    ) -> Result<u64, ArcaError> {
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM presigned_urls WHERE expires_at <= ?1",
                    params![now],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("purge_expired_presigned_urls: {e}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use arca_core::store::presigned_url::{PresignedUrlRecord, PresignedUrlStore};
    use chrono::{Duration, Utc};

    use crate::sqlite::SqliteStore;

    fn sample_record(bucket: &str, key: &str) -> PresignedUrlRecord {
        PresignedUrlRecord {
            id: uuid::Uuid::new_v4().to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            method: "GET".to_string(),
            expires_seconds: 3600,
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::hours(1),
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
        }
    }

    #[tokio::test]
    async fn presigned_url_insert_and_list() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        store.insert_presigned_url(&sample_record("my-bucket", "file.txt")).await.unwrap();
        store.insert_presigned_url(&sample_record("my-bucket", "other.txt")).await.unwrap();
        store.insert_presigned_url(&sample_record("other-bucket", "doc.pdf")).await.unwrap();

        let entries = store.list_presigned_urls("my-bucket").await.unwrap();
        assert_eq!(entries.len(), 2);

        let entries = store.list_presigned_urls("other-bucket").await.unwrap();
        assert_eq!(entries.len(), 1);

        let entries = store.list_presigned_urls("no-such-bucket").await.unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[tokio::test]
    async fn presigned_url_expired_not_listed() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        let mut expired = sample_record("bucket", "old.txt");
        expired.expires_at = Utc::now() - Duration::hours(1);
        store.insert_presigned_url(&expired).await.unwrap();

        store.insert_presigned_url(&sample_record("bucket", "active.txt")).await.unwrap();

        let entries = store.list_presigned_urls("bucket").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "active.txt");
    }

    #[tokio::test]
    async fn presigned_url_delete() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        let record = sample_record("bucket", "file.txt");
        let id = record.id.clone();
        store.insert_presigned_url(&record).await.unwrap();

        assert!(store.delete_presigned_url(&id).await.unwrap());
        assert!(!store.delete_presigned_url(&id).await.unwrap()); // already deleted

        let entries = store.list_presigned_urls("bucket").await.unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[tokio::test]
    async fn presigned_url_purge_expired() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        let mut expired = sample_record("bucket", "old.txt");
        expired.expires_at = Utc::now() - Duration::hours(1);
        store.insert_presigned_url(&expired).await.unwrap();

        store.insert_presigned_url(&sample_record("bucket", "active.txt")).await.unwrap();

        let purged = store.purge_expired_presigned_urls().await.unwrap();
        assert_eq!(purged, 1);

        let entries = store.list_presigned_urls("bucket").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "active.txt");
    }

    #[tokio::test]
    async fn presigned_url_multiple_per_object() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        // Two presigned URLs for the same object (different methods/expiry)
        let mut r1 = sample_record("bucket", "file.txt");
        r1.method = "GET".to_string();
        store.insert_presigned_url(&r1).await.unwrap();

        let mut r2 = sample_record("bucket", "file.txt");
        r2.method = "PUT".to_string();
        store.insert_presigned_url(&r2).await.unwrap();

        let entries = store.list_presigned_urls("bucket").await.unwrap();
        assert_eq!(entries.len(), 2);
    }
}
