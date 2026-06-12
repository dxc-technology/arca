//! PostgreSQL implementation of the `AuditStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::audit::{AuditEntry, AuditFilter, AuditStore};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

/// Converts a PostgreSQL row to an `AuditEntry`.
fn row_to_audit_entry(row: &sqlx_postgres::PgRow) -> AuditEntry {
    AuditEntry {
        id: row.get::<i64, _>("id"),
        timestamp: row.get::<DateTime<Utc>, _>("timestamp"),
        request_id: row.get("request_id"),
        operation: row.get("operation"),
        bucket: row.get("bucket"),
        key: row.get("key"),
        version_id: row.get("version_id"),
        user_id: row.get("user_id"),
        access_key_id: row.get("access_key_id"),
        source_ip: row.get("source_ip"),
        http_method: row.get("http_method"),
        http_status: row.get::<i32, _>("http_status") as u16,
        error_code: row.get("error_code"),
        bytes_sent: row.get::<i64, _>("bytes_sent") as u64,
        bytes_received: row.get::<i64, _>("bytes_received") as u64,
        duration_ms: row.get::<i64, _>("duration_ms") as u64,
        user_agent: row.get("user_agent"),
    }
}

#[async_trait::async_trait]
impl AuditStore for PgStore {
    async fn insert_audit_entry(&self, entry: &AuditEntry) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO audit_log (timestamp, request_id, operation, bucket, key,
                version_id, user_id, access_key_id, source_ip, http_method,
                http_status, error_code, bytes_sent, bytes_received, duration_ms,
                user_agent)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
        )
        .bind(entry.timestamp)
        .bind(&entry.request_id)
        .bind(&entry.operation)
        .bind(&entry.bucket)
        .bind(&entry.key)
        .bind(&entry.version_id)
        .bind(&entry.user_id)
        .bind(&entry.access_key_id)
        .bind(&entry.source_ip)
        .bind(&entry.http_method)
        .bind(entry.http_status as i32)
        .bind(&entry.error_code)
        .bind(entry.bytes_sent as i64)
        .bind(entry.bytes_received as i64)
        .bind(entry.duration_ms as i64)
        .bind(&entry.user_agent)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("insert_audit_entry: {e}")))?;

        Ok(())
    }

    async fn insert_audit_entries_batch(&self, entries: &[AuditEntry]) -> Result<(), ArcaError> {
        let mut tx = self.pool.begin().await
            .map_err(|e| ArcaError::Internal(format!("insert_audit_entries_batch: {e}")))?;

        for entry in entries {
            sqlx_core::query::query(
                "INSERT INTO audit_log (timestamp, request_id, operation, bucket, key,
                    version_id, user_id, access_key_id, source_ip, http_method,
                    http_status, error_code, bytes_sent, bytes_received, duration_ms,
                    user_agent)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            )
            .bind(entry.timestamp)
            .bind(&entry.request_id)
            .bind(&entry.operation)
            .bind(&entry.bucket)
            .bind(&entry.key)
            .bind(&entry.version_id)
            .bind(&entry.user_id)
            .bind(&entry.access_key_id)
            .bind(&entry.source_ip)
            .bind(&entry.http_method)
            .bind(entry.http_status as i32)
            .bind(&entry.error_code)
            .bind(entry.bytes_sent as i64)
            .bind(entry.bytes_received as i64)
            .bind(entry.duration_ms as i64)
            .bind(&entry.user_agent)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("insert_audit_entries_batch: {e}")))?;
        }

        tx.commit().await
            .map_err(|e| ArcaError::Internal(format!("insert_audit_entries_batch: {e}")))?;

        Ok(())
    }

    async fn list_audit_entries(
        &self,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEntry>, ArcaError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut param_idx = 1u32;

        // Track which optional params are bound so we can bind them in order below.
        let bucket = filter.bucket.clone();
        let operation = filter.operation.clone();
        let user_id = filter.user_id.clone();
        let from = filter.from;
        let to = filter.to;

        if bucket.is_some() {
            conditions.push(format!("bucket = ${param_idx}"));
            param_idx += 1;
        }
        if operation.is_some() {
            conditions.push(format!("operation = ${param_idx}"));
            param_idx += 1;
        }
        if user_id.is_some() {
            conditions.push(format!("user_id = ${param_idx}"));
            param_idx += 1;
        }
        if from.is_some() {
            conditions.push(format!("timestamp >= ${param_idx}"));
            param_idx += 1;
        }
        if to.is_some() {
            conditions.push(format!("timestamp <= ${param_idx}"));
            param_idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let limit = if filter.limit == 0 { 100 } else { filter.limit };
        let offset = filter.offset;

        let sql = format!(
            "SELECT * FROM audit_log {where_clause} ORDER BY timestamp DESC LIMIT ${param_idx} OFFSET ${next}",
            next = param_idx + 1,
        );

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        if let Some(ref b) = bucket {
            query = query.bind(b);
        }
        if let Some(ref o) = operation {
            query = query.bind(o);
        }
        if let Some(ref u) = user_id {
            query = query.bind(u);
        }
        if let Some(f) = from {
            query = query.bind(f);
        }
        if let Some(t) = to {
            query = query.bind(t);
        }
        query = query.bind(limit as i64).bind(offset as i64);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_audit_entries: {e}")))?;

        Ok(rows.iter().map(row_to_audit_entry).collect())
    }

    async fn count_audit_entries(&self, filter: &AuditFilter) -> Result<u64, ArcaError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut param_idx = 1u32;

        let bucket = filter.bucket.clone();
        let operation = filter.operation.clone();
        let user_id = filter.user_id.clone();
        let from = filter.from;
        let to = filter.to;

        if bucket.is_some() {
            conditions.push(format!("bucket = ${param_idx}"));
            param_idx += 1;
        }
        if operation.is_some() {
            conditions.push(format!("operation = ${param_idx}"));
            param_idx += 1;
        }
        if user_id.is_some() {
            conditions.push(format!("user_id = ${param_idx}"));
            param_idx += 1;
        }
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

        let sql = format!("SELECT COUNT(*) FROM audit_log {where_clause}");

        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        if let Some(ref b) = bucket {
            query = query.bind(b);
        }
        if let Some(ref o) = operation {
            query = query.bind(o);
        }
        if let Some(ref u) = user_id {
            query = query.bind(u);
        }
        if let Some(f) = from {
            query = query.bind(f);
        }
        if let Some(t) = to {
            query = query.bind(t);
        }

        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("count_audit_entries: {e}")))?;

        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn purge_audit_entries(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let result = sqlx_core::query::query("DELETE FROM audit_log WHERE timestamp < $1")
            .bind(before)
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("purge_audit_entries: {e}")))?;

        Ok(result.rows_affected())
    }
}
