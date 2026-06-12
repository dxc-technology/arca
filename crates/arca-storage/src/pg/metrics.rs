//! PostgreSQL implementation of the `MetricsStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::metrics::{MetricsSnapshot, MetricsStore};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

/// Converts a PostgreSQL row to a `MetricsSnapshot`.
fn row_to_metrics_snapshot(row: &sqlx_postgres::PgRow) -> MetricsSnapshot {
    MetricsSnapshot {
        id: row.get::<i64, _>("id"),
        timestamp: row.get::<DateTime<Utc>, _>("timestamp"),
        bucket_count: row.get::<i32, _>("bucket_count") as u64,
        object_count: row.get::<i64, _>("object_count") as u64,
        total_size_bytes: row.get::<i64, _>("total_size_bytes") as u64,
        disk_total_bytes: row.get::<Option<i64>, _>("disk_total_bytes").map(|v| v as u64),
        disk_available_bytes: row.get::<Option<i64>, _>("disk_available_bytes").map(|v| v as u64),
        active_connections: row.get::<i32, _>("active_connections") as u64,
    }
}

#[async_trait::async_trait]
impl MetricsStore for PgStore {
    async fn insert_metrics_snapshot(
        &self,
        snapshot: &MetricsSnapshot,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO metrics_snapshot (timestamp, bucket_count, object_count,
                total_size_bytes, disk_total_bytes, disk_available_bytes,
                active_connections)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(snapshot.timestamp)
        .bind(snapshot.bucket_count as i32)
        .bind(snapshot.object_count as i64)
        .bind(snapshot.total_size_bytes as i64)
        .bind(snapshot.disk_total_bytes.map(|v| v as i64))
        .bind(snapshot.disk_available_bytes.map(|v| v as i64))
        .bind(snapshot.active_connections as i32)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("insert_metrics_snapshot: {e}")))?;

        Ok(())
    }

    async fn list_metrics_snapshots(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: u32,
    ) -> Result<Vec<MetricsSnapshot>, ArcaError> {
        let limit = if limit == 0 { 500 } else { limit };

        let mut conditions: Vec<String> = Vec::new();
        let mut param_idx = 1u32;

        if from.is_some() {
            conditions.push(format!("timestamp >= ${param_idx}"));
            param_idx += 1;
        }
        if to.is_some() {
            conditions.push(format!("timestamp <= ${param_idx}"));
            let _ = param_idx;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        // Count total matching rows to decide if downsampling is needed
        let count_sql =
            format!("SELECT COUNT(*)::bigint FROM metrics_snapshot {where_clause}");
        let mut count_query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(count_sql.as_str()));
        if let Some(f) = from {
            count_query = count_query.bind(f);
        }
        if let Some(t) = to {
            count_query = count_query.bind(t);
        }
        let count_row = count_query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_metrics_snapshots count: {e}")))?;
        let total: i64 = count_row.get(0);

        let limit_i64 = limit as i64;

        let sql = if total <= limit_i64 {
            format!(
                "SELECT * FROM metrics_snapshot {where_clause} ORDER BY timestamp DESC"
            )
        } else {
            let step = total / limit_i64;
            format!(
                "WITH ranked AS (\
                   SELECT *, ROW_NUMBER() OVER (ORDER BY timestamp ASC) AS rn \
                   FROM metrics_snapshot {where_clause}\
                 ) \
                 SELECT id, timestamp, bucket_count, object_count, total_size_bytes, \
                        disk_total_bytes, disk_available_bytes, active_connections \
                 FROM ranked \
                 WHERE (rn - 1) % {step} = 0 \
                 ORDER BY timestamp DESC"
            )
        };

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        if let Some(f) = from {
            query = query.bind(f);
        }
        if let Some(t) = to {
            query = query.bind(t);
        }

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_metrics_snapshots: {e}")))?;

        Ok(rows.iter().map(row_to_metrics_snapshot).collect())
    }

    async fn purge_metrics_snapshots(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, ArcaError> {
        let result = sqlx_core::query::query("DELETE FROM metrics_snapshot WHERE timestamp < $1")
            .bind(before)
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("purge_metrics_snapshots: {e}")))?;

        Ok(result.rows_affected())
    }
}
