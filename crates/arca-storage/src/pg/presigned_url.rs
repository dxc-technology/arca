//! PostgreSQL implementation of the `PresignedUrlStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::presigned_url::{PresignedUrlRecord, PresignedUrlStore};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

fn row_to_presigned_url(row: &sqlx_postgres::PgRow) -> PresignedUrlRecord {
    PresignedUrlRecord {
        id: row.get("id"),
        bucket: row.get("bucket"),
        key: row.get("key"),
        method: row.get("method"),
        expires_seconds: row.get::<i64, _>("expires_seconds") as u64,
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        expires_at: row.get::<DateTime<Utc>, _>("expires_at"),
        access_key_id: row.get("access_key_id"),
    }
}

#[async_trait::async_trait]
impl PresignedUrlStore for PgStore {
    async fn insert_presigned_url(
        &self,
        record: &PresignedUrlRecord,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO presigned_urls
                (id, bucket, key, method, expires_seconds,
                 created_at, expires_at, access_key_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(&record.id)
        .bind(&record.bucket)
        .bind(&record.key)
        .bind(&record.method)
        .bind(record.expires_seconds as i64)
        .bind(record.created_at)
        .bind(record.expires_at)
        .bind(&record.access_key_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("insert_presigned_url: {e}")))?;

        Ok(())
    }

    async fn list_presigned_urls(
        &self,
        bucket: &str,
    ) -> Result<Vec<PresignedUrlRecord>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT * FROM presigned_urls
             WHERE bucket = $1 AND expires_at > NOW()
             ORDER BY created_at DESC",
        )
        .bind(bucket)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_presigned_urls: {e}")))?;

        Ok(rows.iter().map(row_to_presigned_url).collect())
    }

    async fn delete_presigned_url(
        &self,
        id: &str,
    ) -> Result<bool, ArcaError> {
        let result = sqlx_core::query::query(
            "DELETE FROM presigned_urls WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("delete_presigned_url: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn purge_expired_presigned_urls(
        &self,
    ) -> Result<u64, ArcaError> {
        let result = sqlx_core::query::query(
            "DELETE FROM presigned_urls WHERE expires_at <= NOW()",
        )
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("purge_expired_presigned_urls: {e}")))?;

        Ok(result.rows_affected())
    }
}
