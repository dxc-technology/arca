//! PostgreSQL implementation of the `ReplicationStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::replication::{JournalEntry, JournalFilter, ReplicationStore};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

fn row_to_entry(row: &sqlx_postgres::PgRow) -> JournalEntry {
    JournalEntry {
        id: row.get("id"),
        bucket: row.get("bucket"),
        key: row.get("key"),
        version_id: row.get("version_id"),
        rule_id: row.get("rule_id"),
        event_type: row.get("event_type"),
        destination_endpoint: row.get("destination_endpoint"),
        destination_bucket: row.get("destination_bucket"),
        status: row.get("status"),
        attempts: row.get::<i32, _>("attempts") as u32,
        last_error: row.get("last_error"),
        next_retry_at: row.get::<DateTime<Utc>, _>("next_retry_at"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
    }
}

#[async_trait::async_trait]
impl ReplicationStore for PgStore {
    async fn insert_journal_entry(&self, entry: &JournalEntry) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO replication_journal (
                id, bucket, key, version_id, rule_id, event_type,
                destination_endpoint, destination_bucket, status,
                attempts, last_error, next_retry_at, created_at, updated_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)",
        )
        .bind(&entry.id)
        .bind(&entry.bucket)
        .bind(&entry.key)
        .bind(&entry.version_id)
        .bind(&entry.rule_id)
        .bind(&entry.event_type)
        .bind(&entry.destination_endpoint)
        .bind(&entry.destination_bucket)
        .bind(&entry.status)
        .bind(entry.attempts as i32)
        .bind(&entry.last_error)
        .bind(entry.next_retry_at)
        .bind(entry.created_at)
        .bind(entry.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("insert_journal_entry: {e}")))?;
        Ok(())
    }

    async fn claim_batch(&self, limit: u32) -> Result<Vec<JournalEntry>, ArcaError> {
        let rows = sqlx_core::query::query(
            "UPDATE replication_journal
             SET status = 'in_flight', updated_at = NOW()
             WHERE id IN (
                 SELECT id FROM replication_journal
                 WHERE status IN ('pending','failed') AND next_retry_at <= NOW()
                 ORDER BY next_retry_at ASC
                 LIMIT $1
                 FOR UPDATE SKIP LOCKED
             )
             RETURNING *",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("claim_batch: {e}")))?;
        Ok(rows.iter().map(row_to_entry).collect())
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        attempts: u32,
        last_error: Option<&str>,
        next_retry_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "UPDATE replication_journal
             SET status = $1, attempts = $2, last_error = $3,
                 next_retry_at = $4, updated_at = NOW()
             WHERE id = $5",
        )
        .bind(status)
        .bind(attempts as i32)
        .bind(last_error)
        .bind(next_retry_at)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("update_status: {e}")))?;
        Ok(())
    }

    async fn list_journal(
        &self,
        filter: &JournalFilter,
    ) -> Result<Vec<JournalEntry>, ArcaError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut idx = 1u32;

        let bucket = filter.bucket.clone();
        let status = filter.status.clone();
        let rule_id = filter.rule_id.clone();

        if bucket.is_some() {
            conditions.push(format!("bucket = ${idx}"));
            idx += 1;
        }
        if status.is_some() {
            conditions.push(format!("status = ${idx}"));
            idx += 1;
        }
        if rule_id.is_some() {
            conditions.push(format!("rule_id = ${idx}"));
            idx += 1;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let limit = if filter.limit == 0 { 100 } else { filter.limit };
        let offset = filter.offset;
        let sql = format!(
            "SELECT * FROM replication_journal {where_clause}
             ORDER BY created_at DESC
             LIMIT ${} OFFSET ${}",
            idx,
            idx + 1
        );

        let mut q = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        if let Some(ref b) = bucket {
            q = q.bind(b);
        }
        if let Some(ref s) = status {
            q = q.bind(s);
        }
        if let Some(ref r) = rule_id {
            q = q.bind(r);
        }
        q = q.bind(limit as i64).bind(offset as i64);

        let rows = q
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_journal: {e}")))?;

        Ok(rows.iter().map(row_to_entry).collect())
    }

    async fn count_journal(&self, filter: &JournalFilter) -> Result<u64, ArcaError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut idx = 1u32;

        let bucket = filter.bucket.clone();
        let status = filter.status.clone();
        let rule_id = filter.rule_id.clone();

        if bucket.is_some() {
            conditions.push(format!("bucket = ${idx}"));
            idx += 1;
        }
        if status.is_some() {
            conditions.push(format!("status = ${idx}"));
            idx += 1;
        }
        if rule_id.is_some() {
            conditions.push(format!("rule_id = ${idx}"));
            let _ = idx;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };
        let sql = format!("SELECT COUNT(*) FROM replication_journal {where_clause}");

        let mut q = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        if let Some(ref b) = bucket {
            q = q.bind(b);
        }
        if let Some(ref s) = status {
            q = q.bind(s);
        }
        if let Some(ref r) = rule_id {
            q = q.bind(r);
        }

        let row = q
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("count_journal: {e}")))?;
        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn purge_completed(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let result = sqlx_core::query::query(
            "DELETE FROM replication_journal
             WHERE status = 'completed' AND updated_at < $1",
        )
        .bind(before)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("purge_completed: {e}")))?;
        Ok(result.rows_affected())
    }

    async fn purge_all_older(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let result =
            sqlx_core::query::query("DELETE FROM replication_journal WHERE updated_at < $1")
                .bind(before)
                .execute(&self.pool)
                .await
                .map_err(|e| ArcaError::Internal(format!("purge_all_older: {e}")))?;
        Ok(result.rows_affected())
    }

    async fn set_object_replication_status(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<(), ArcaError> {
        match version_id {
            Some(vid) => {
                sqlx_core::query::query(
                    "UPDATE objects SET replication_status = $1
                     WHERE bucket = $2 AND key = $3 AND version_id = $4",
                )
                .bind(status)
                .bind(bucket)
                .bind(key)
                .bind(vid)
                .execute(&self.pool)
                .await
                .map_err(|e| {
                    ArcaError::Internal(format!("set_object_replication_status: {e}"))
                })?;
            }
            None => {
                sqlx_core::query::query(
                    "UPDATE objects SET replication_status = $1
                     WHERE bucket = $2 AND key = $3
                       AND version_id IS NULL AND is_latest = TRUE",
                )
                .bind(status)
                .bind(bucket)
                .bind(key)
                .execute(&self.pool)
                .await
                .map_err(|e| {
                    ArcaError::Internal(format!("set_object_replication_status: {e}"))
                })?;
            }
        }
        Ok(())
    }
}
