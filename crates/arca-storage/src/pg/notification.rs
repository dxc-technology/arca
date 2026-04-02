//! PostgreSQL implementation of the `NotificationStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::notification::{
    NotificationEventFilter, NotificationEventRecord, NotificationStore,
};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

/// Converts a PostgreSQL row to a `NotificationEventRecord`.
fn row_to_notification_event(row: &sqlx_postgres::PgRow) -> NotificationEventRecord {
    NotificationEventRecord {
        id: row.get("id"),
        bucket: row.get("bucket"),
        key: row.get("key"),
        event_name: row.get("event_name"),
        event_time: row.get::<DateTime<Utc>, _>("event_time"),
        payload: row.get("payload"),
        destination_url: row.get("destination_url"),
        configuration_id: row.get("configuration_id"),
        delivery_status: row.get("delivery_status"),
        delivery_attempts: row.get::<i32, _>("delivery_attempts") as u32,
        last_error: row.get("last_error"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
    }
}

#[async_trait::async_trait]
impl NotificationStore for PgStore {
    async fn insert_notification_event(
        &self,
        event: &NotificationEventRecord,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO notification_events
                (id, bucket, key, event_name, event_time, payload,
                 destination_url, configuration_id, delivery_status,
                 delivery_attempts, last_error, created_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(&event.id)
        .bind(&event.bucket)
        .bind(&event.key)
        .bind(&event.event_name)
        .bind(event.event_time)
        .bind(&event.payload)
        .bind(&event.destination_url)
        .bind(&event.configuration_id)
        .bind(&event.delivery_status)
        .bind(event.delivery_attempts as i32)
        .bind(&event.last_error)
        .bind(event.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("insert_notification_event: {e}")))?;

        Ok(())
    }

    async fn update_notification_event_status(
        &self,
        id: &str,
        status: &str,
        attempts: u32,
        last_error: Option<&str>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "UPDATE notification_events
             SET delivery_status = $1, delivery_attempts = $2, last_error = $3
             WHERE id = $4",
        )
        .bind(status)
        .bind(attempts as i32)
        .bind(last_error)
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("update_notification_event_status: {e}")))?;

        Ok(())
    }

    async fn list_notification_events(
        &self,
        filter: &NotificationEventFilter,
    ) -> Result<Vec<NotificationEventRecord>, ArcaError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut param_idx = 1u32;

        let bucket = filter.bucket.clone();
        let event_name = filter.event_name.clone();
        let delivery_status = filter.delivery_status.clone();

        if bucket.is_some() {
            conditions.push(format!("bucket = ${param_idx}"));
            param_idx += 1;
        }
        if event_name.is_some() {
            conditions.push(format!("event_name = ${param_idx}"));
            param_idx += 1;
        }
        if delivery_status.is_some() {
            conditions.push(format!("delivery_status = ${param_idx}"));
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
            "SELECT * FROM notification_events {where_clause} ORDER BY created_at DESC LIMIT ${param_idx} OFFSET ${next}",
            next = param_idx + 1,
        );

        let mut query = sqlx_core::query::query(&sql);
        if let Some(ref b) = bucket {
            query = query.bind(b);
        }
        if let Some(ref e) = event_name {
            query = query.bind(e);
        }
        if let Some(ref s) = delivery_status {
            query = query.bind(s);
        }
        query = query.bind(limit as i64).bind(offset as i64);

        let rows = query
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("list_notification_events: {e}")))?;

        Ok(rows.iter().map(row_to_notification_event).collect())
    }

    async fn count_notification_events(
        &self,
        filter: &NotificationEventFilter,
    ) -> Result<u64, ArcaError> {
        let mut conditions: Vec<String> = Vec::new();
        let mut param_idx = 1u32;

        let bucket = filter.bucket.clone();
        let event_name = filter.event_name.clone();
        let delivery_status = filter.delivery_status.clone();

        if bucket.is_some() {
            conditions.push(format!("bucket = ${param_idx}"));
            param_idx += 1;
        }
        if event_name.is_some() {
            conditions.push(format!("event_name = ${param_idx}"));
            param_idx += 1;
        }
        if delivery_status.is_some() {
            conditions.push(format!("delivery_status = ${param_idx}"));
            let _ = param_idx;
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!("SELECT COUNT(*) FROM notification_events {where_clause}");

        let mut query = sqlx_core::query::query(&sql);
        if let Some(ref b) = bucket {
            query = query.bind(b);
        }
        if let Some(ref e) = event_name {
            query = query.bind(e);
        }
        if let Some(ref s) = delivery_status {
            query = query.bind(s);
        }

        let row = query
            .fetch_one(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("count_notification_events: {e}")))?;

        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn purge_notification_events(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, ArcaError> {
        let result =
            sqlx_core::query::query("DELETE FROM notification_events WHERE created_at < $1")
                .bind(before)
                .execute(&self.pool)
                .await
                .map_err(|e| ArcaError::Internal(format!("purge_notification_events: {e}")))?;

        Ok(result.rows_affected())
    }
}
