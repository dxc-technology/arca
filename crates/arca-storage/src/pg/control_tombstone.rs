//! `ControlTombstoneStore` implementation for `PgStore`.

use arca_core::error::ArcaError;
use arca_core::store::{ControlTombstone, ControlTombstoneStore};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

#[async_trait::async_trait]
impl ControlTombstoneStore for PgStore {
    async fn record_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO control_tombstones (entity_type, entity_key, deleted_at)
             VALUES ($1, $2, NOW())
             ON CONFLICT (entity_type, entity_key) DO UPDATE SET deleted_at = NOW()",
        )
        .bind(entity_type)
        .bind(entity_key)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("record_control_tombstone: {e}")))?;
        Ok(())
    }

    async fn list_control_tombstones(&self) -> Result<Vec<ControlTombstone>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT entity_type, entity_key, deleted_at FROM control_tombstones",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_control_tombstones: {e}")))?;

        Ok(rows.iter().map(row_to_tombstone).collect())
    }

    async fn delete_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<bool, ArcaError> {
        let result = sqlx_core::query::query(
            "DELETE FROM control_tombstones WHERE entity_type = $1 AND entity_key = $2",
        )
        .bind(entity_type)
        .bind(entity_key)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("delete_control_tombstone: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn purge_control_tombstones(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let result = sqlx_core::query::query("DELETE FROM control_tombstones WHERE deleted_at < $1")
            .bind(before)
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("purge_control_tombstones: {e}")))?;

        Ok(result.rows_affected())
    }
}

/// Converts a PostgreSQL row to a `ControlTombstone`.
fn row_to_tombstone(row: &sqlx_postgres::PgRow) -> ControlTombstone {
    ControlTombstone {
        entity_type: row.get("entity_type"),
        entity_key: row.get("entity_key"),
        deleted_at: row.get::<DateTime<Utc>, _>("deleted_at"),
    }
}
