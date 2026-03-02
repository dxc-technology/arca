//! `MetadataStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::MetadataStore;
use arca_core::types::BucketInfo;
use chrono::DateTime;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl MetadataStore for SqliteStore {
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

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
}
