//! SQLite implementation of the `NotificationStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::notification::{
    NotificationEventFilter, NotificationEventRecord, NotificationStore,
};
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

/// Build a WHERE clause and string parameter values from a `NotificationEventFilter`.
fn build_filter_clause(filter: &NotificationEventFilter) -> (String, Vec<String>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut params_vec: Vec<String> = Vec::new();
    let mut idx = 1;

    if let Some(ref bucket) = filter.bucket {
        conditions.push(format!("bucket = ?{idx}"));
        params_vec.push(bucket.clone());
        idx += 1;
    }
    if let Some(ref event_name) = filter.event_name {
        conditions.push(format!("event_name = ?{idx}"));
        params_vec.push(event_name.clone());
        idx += 1;
    }
    if let Some(ref status) = filter.delivery_status {
        conditions.push(format!("delivery_status = ?{idx}"));
        params_vec.push(status.clone());
        let _ = idx;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    (where_clause, params_vec)
}

fn row_to_notification_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<NotificationEventRecord> {
    Ok(NotificationEventRecord {
        id: row.get("id")?,
        bucket: row.get("bucket")?,
        key: row.get("key")?,
        event_name: row.get("event_name")?,
        event_time: row
            .get::<_, String>("event_time")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        payload: row.get("payload")?,
        destination_url: row.get("destination_url")?,
        configuration_id: row.get("configuration_id")?,
        delivery_status: row.get("delivery_status")?,
        delivery_attempts: row.get::<_, u32>("delivery_attempts")?,
        last_error: row.get("last_error")?,
        created_at: row
            .get::<_, String>("created_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        connector_type: row
            .get::<_, String>("connector_type")
            .unwrap_or_else(|_| "webhook".to_string()),
    })
}

#[async_trait::async_trait]
impl NotificationStore for SqliteStore {
    async fn insert_notification_event(
        &self,
        event: &NotificationEventRecord,
    ) -> Result<(), ArcaError> {
        let event = event.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO notification_events
                        (id, bucket, key, event_name, event_time, payload,
                         destination_url, configuration_id, delivery_status,
                         delivery_attempts, last_error, created_at, connector_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    params![
                        event.id,
                        event.bucket,
                        event.key,
                        event.event_name,
                        event.event_time.to_rfc3339(),
                        event.payload,
                        event.destination_url,
                        event.configuration_id,
                        event.delivery_status,
                        event.delivery_attempts,
                        event.last_error,
                        event.created_at.to_rfc3339(),
                        event.connector_type,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("insert_notification_event: {e}"))
            })
    }

    async fn update_notification_event_status(
        &self,
        id: &str,
        status: &str,
        attempts: u32,
        last_error: Option<&str>,
    ) -> Result<(), ArcaError> {
        let id = id.to_string();
        let status = status.to_string();
        let last_error = last_error.map(|s| s.to_string());
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE notification_events
                     SET delivery_status = ?1, delivery_attempts = ?2, last_error = ?3
                     WHERE id = ?4",
                    params![status, attempts, last_error, id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("update_notification_event_status: {e}"))
            })
    }

    async fn list_notification_events(
        &self,
        filter: &NotificationEventFilter,
    ) -> Result<Vec<NotificationEventRecord>, ArcaError> {
        let (where_clause, params_vec) = build_filter_clause(filter);
        let limit = if filter.limit == 0 { 100 } else { filter.limit };
        let offset = filter.offset;

        self.conn
            .call(move |conn| {
                let sql = format!(
                    "SELECT * FROM notification_events {where_clause} ORDER BY created_at DESC LIMIT {limit} OFFSET {offset}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
                let rows = stmt.query_map(params_refs.as_slice(), row_to_notification_event)?;
                let mut entries = Vec::new();
                for row in rows {
                    entries.push(row?);
                }
                Ok(entries)
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("list_notification_events: {e}"))
            })
    }

    async fn count_notification_events(
        &self,
        filter: &NotificationEventFilter,
    ) -> Result<u64, ArcaError> {
        let (where_clause, params_vec) = build_filter_clause(filter);

        self.conn
            .call(move |conn| {
                let sql =
                    format!("SELECT COUNT(*) FROM notification_events {where_clause}");
                let mut stmt = conn.prepare(&sql)?;
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
                let count: i64 = stmt.query_row(params_refs.as_slice(), |row| row.get(0))?;
                Ok(count as u64)
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("count_notification_events: {e}"))
            })
    }

    async fn purge_notification_events(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, ArcaError> {
        let before_str = before.to_rfc3339();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM notification_events WHERE created_at < ?1",
                    params![before_str],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("purge_notification_events: {e}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use arca_core::store::notification::{
        NotificationEventFilter, NotificationEventRecord, NotificationStore,
    };
    use chrono::{Duration, Utc};

    use crate::sqlite::SqliteStore;

    fn sample_event(bucket: &str, key: &str, event_name: &str) -> NotificationEventRecord {
        NotificationEventRecord {
            id: uuid::Uuid::new_v4().to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            event_name: event_name.to_string(),
            event_time: Utc::now(),
            payload: r#"{"Records":[]}"#.to_string(),
            destination_url: "http://example.com/webhook".to_string(),
            configuration_id: "hook1".to_string(),
            delivery_status: "pending".to_string(),
            delivery_attempts: 0,
            last_error: None,
            created_at: Utc::now(),
            connector_type: "webhook".to_string(),
        }
    }

    #[tokio::test]
    async fn notification_insert_and_list() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        store
            .insert_notification_event(&sample_event(
                "my-bucket",
                "file.txt",
                "s3:ObjectCreated:Put",
            ))
            .await
            .unwrap();
        store
            .insert_notification_event(&sample_event(
                "my-bucket",
                "other.txt",
                "s3:ObjectRemoved:Delete",
            ))
            .await
            .unwrap();
        store
            .insert_notification_event(&sample_event(
                "other-bucket",
                "doc.pdf",
                "s3:ObjectCreated:Put",
            ))
            .await
            .unwrap();

        // List all
        let filter = NotificationEventFilter {
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(entries.len(), 3);

        // Count all
        let count = store
            .count_notification_events(&NotificationEventFilter::default())
            .await
            .unwrap();
        assert_eq!(count, 3);

        // Filter by bucket
        let filter = NotificationEventFilter {
            bucket: Some("my-bucket".to_string()),
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(entries.len(), 2);

        // Filter by event name
        let filter = NotificationEventFilter {
            event_name: Some("s3:ObjectRemoved:Delete".to_string()),
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(entries.len(), 1);

        // Filter by delivery status
        let filter = NotificationEventFilter {
            delivery_status: Some("pending".to_string()),
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(entries.len(), 3);
    }

    #[tokio::test]
    async fn notification_update_status() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        let event = sample_event("bucket", "key.txt", "s3:ObjectCreated:Put");
        let id = event.id.clone();
        store.insert_notification_event(&event).await.unwrap();

        // Update to delivered
        store
            .update_notification_event_status(&id, "delivered", 1, None)
            .await
            .unwrap();

        let filter = NotificationEventFilter {
            delivery_status: Some("delivered".to_string()),
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].delivery_attempts, 1);
        assert!(entries[0].last_error.is_none());

        // Update to failed
        store
            .update_notification_event_status(&id, "failed", 3, Some("connection refused"))
            .await
            .unwrap();

        let filter = NotificationEventFilter {
            delivery_status: Some("failed".to_string()),
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].delivery_attempts, 3);
        assert_eq!(entries[0].last_error.as_deref(), Some("connection refused"));
    }

    #[tokio::test]
    async fn notification_purge() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        let mut old_event = sample_event("bucket", "old.txt", "s3:ObjectCreated:Put");
        old_event.created_at = Utc::now() - Duration::days(100);
        store.insert_notification_event(&old_event).await.unwrap();

        store
            .insert_notification_event(&sample_event(
                "bucket",
                "new.txt",
                "s3:ObjectCreated:Put",
            ))
            .await
            .unwrap();

        let cutoff = Utc::now() - Duration::days(30);
        let purged = store.purge_notification_events(cutoff).await.unwrap();
        assert_eq!(purged, 1);

        let count = store
            .count_notification_events(&NotificationEventFilter::default())
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn notification_pagination() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        for i in 0..10 {
            let mut event = sample_event("bucket", &format!("file-{i}.txt"), "s3:ObjectCreated:Put");
            event.id = format!("event-{i:03}");
            store.insert_notification_event(&event).await.unwrap();
        }

        let filter = NotificationEventFilter {
            limit: 3,
            offset: 0,
            ..Default::default()
        };
        let page1 = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(page1.len(), 3);

        let filter = NotificationEventFilter {
            limit: 3,
            offset: 3,
            ..Default::default()
        };
        let page2 = store.list_notification_events(&filter).await.unwrap();
        assert_eq!(page2.len(), 3);

        let count = store
            .count_notification_events(&NotificationEventFilter::default())
            .await
            .unwrap();
        assert_eq!(count, 10);
    }
}
