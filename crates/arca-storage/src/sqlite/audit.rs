//! SQLite implementation of the `AuditStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::audit::{AuditEntry, AuditFilter, AuditStore};
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

/// Build a WHERE clause and string parameter values from an `AuditFilter`.
/// All values are converted to strings for Send-safety across the tokio-rusqlite boundary.
fn build_filter_clause(filter: &AuditFilter) -> (String, Vec<String>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut params_vec: Vec<String> = Vec::new();
    let mut idx = 1;

    if let Some(ref bucket) = filter.bucket {
        conditions.push(format!("bucket = ?{idx}"));
        params_vec.push(bucket.clone());
        idx += 1;
    }
    if let Some(ref operation) = filter.operation {
        conditions.push(format!("operation = ?{idx}"));
        params_vec.push(operation.clone());
        idx += 1;
    }
    if let Some(ref user_id) = filter.user_id {
        conditions.push(format!("user_id = ?{idx}"));
        params_vec.push(user_id.clone());
        idx += 1;
    }
    if let Some(ref from) = filter.from {
        conditions.push(format!("timestamp >= ?{idx}"));
        params_vec.push(from.to_rfc3339());
        idx += 1;
    }
    if let Some(ref to) = filter.to {
        conditions.push(format!("timestamp <= ?{idx}"));
        params_vec.push(to.to_rfc3339());
        let _ = idx; // suppress unused warning
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };

    (where_clause, params_vec)
}

fn row_to_audit_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditEntry> {
    Ok(AuditEntry {
        id: row.get("id")?,
        timestamp: row
            .get::<_, String>("timestamp")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        request_id: row.get("request_id")?,
        operation: row.get("operation")?,
        bucket: row.get("bucket")?,
        key: row.get("key")?,
        version_id: row.get("version_id")?,
        user_id: row.get("user_id")?,
        access_key_id: row.get("access_key_id")?,
        source_ip: row.get("source_ip")?,
        http_method: row.get("http_method")?,
        http_status: row.get::<_, u32>("http_status")? as u16,
        error_code: row.get("error_code")?,
        bytes_sent: row.get::<_, i64>("bytes_sent")? as u64,
        bytes_received: row.get::<_, i64>("bytes_received")? as u64,
        duration_ms: row.get::<_, i64>("duration_ms")? as u64,
        user_agent: row.get("user_agent")?,
    })
}

#[async_trait::async_trait]
impl AuditStore for SqliteStore {
    async fn insert_audit_entry(&self, entry: &AuditEntry) -> Result<(), ArcaError> {
        let entry = entry.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO audit_log (timestamp, request_id, operation, bucket, key,
                        version_id, user_id, access_key_id, source_ip, http_method,
                        http_status, error_code, bytes_sent, bytes_received, duration_ms,
                        user_agent)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    params![
                        entry.timestamp.to_rfc3339(),
                        entry.request_id,
                        entry.operation,
                        entry.bucket,
                        entry.key,
                        entry.version_id,
                        entry.user_id,
                        entry.access_key_id,
                        entry.source_ip,
                        entry.http_method,
                        entry.http_status as u32,
                        entry.error_code,
                        entry.bytes_sent as i64,
                        entry.bytes_received as i64,
                        entry.duration_ms as i64,
                        entry.user_agent,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("insert_audit_entry: {e}")))
    }

    async fn insert_audit_entries_batch(&self, entries: &[AuditEntry]) -> Result<(), ArcaError> {
        let entries: Vec<AuditEntry> = entries.to_vec();
        self.conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare_cached(
                        "INSERT INTO audit_log (timestamp, request_id, operation, bucket, key,
                            version_id, user_id, access_key_id, source_ip, http_method,
                            http_status, error_code, bytes_sent, bytes_received, duration_ms,
                            user_agent)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
                    )?;
                    for entry in &entries {
                        stmt.execute(params![
                            entry.timestamp.to_rfc3339(),
                            entry.request_id,
                            entry.operation,
                            entry.bucket,
                            entry.key,
                            entry.version_id,
                            entry.user_id,
                            entry.access_key_id,
                            entry.source_ip,
                            entry.http_method,
                            entry.http_status as u32,
                            entry.error_code,
                            entry.bytes_sent as i64,
                            entry.bytes_received as i64,
                            entry.duration_ms as i64,
                            entry.user_agent,
                        ])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("insert_audit_entries_batch: {e}")))
    }

    async fn list_audit_entries(
        &self,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEntry>, ArcaError> {
        let (where_clause, params_vec) = build_filter_clause(filter);
        let limit = if filter.limit == 0 { 100 } else { filter.limit };
        let offset = filter.offset;

        self.read_conn()
            .call(move |conn| {
                let sql = format!(
                    "SELECT * FROM audit_log {where_clause} ORDER BY timestamp DESC LIMIT {limit} OFFSET {offset}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
                let rows = stmt.query_map(params_refs.as_slice(), row_to_audit_entry)?;
                let mut entries = Vec::new();
                for row in rows {
                    entries.push(row?);
                }
                Ok(entries)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_audit_entries: {e}")))
    }

    async fn count_audit_entries(&self, filter: &AuditFilter) -> Result<u64, ArcaError> {
        let (where_clause, params_vec) = build_filter_clause(filter);

        self.read_conn()
            .call(move |conn| {
                let sql = format!("SELECT COUNT(*) FROM audit_log {where_clause}");
                let mut stmt = conn.prepare(&sql)?;
                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p as &dyn rusqlite::types::ToSql).collect();
                let count: i64 = stmt.query_row(params_refs.as_slice(), |row| row.get(0))?;
                Ok(count as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("count_audit_entries: {e}")))
    }

    async fn purge_audit_entries(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let before_str = before.to_rfc3339();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM audit_log WHERE timestamp < ?1",
                    params![before_str],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("purge_audit_entries: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use arca_core::store::audit::{AuditEntry, AuditFilter, AuditStore};
    use chrono::{Duration, Utc};

    use crate::sqlite::SqliteStore;

    fn sample_entry(operation: &str, bucket: Option<&str>) -> AuditEntry {
        AuditEntry {
            id: 0,
            timestamp: Utc::now(),
            request_id: uuid::Uuid::new_v4().to_string(),
            operation: operation.to_string(),
            bucket: bucket.map(|s| s.to_string()),
            key: None,
            version_id: None,
            user_id: Some("root".to_string()),
            access_key_id: Some("AKIAIOSFODNN7EXAMPLE".to_string()),
            source_ip: Some("127.0.0.1".to_string()),
            http_method: "GET".to_string(),
            http_status: 200,
            error_code: None,
            bytes_sent: 1024,
            bytes_received: 0,
            duration_ms: 5,
            user_agent: Some("boto3/1.26".to_string()),
        }
    }

    #[tokio::test]
    async fn audit_insert_and_list() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        store.insert_audit_entry(&sample_entry("ListBuckets", None)).await.unwrap();
        store.insert_audit_entry(&sample_entry("PutObject", Some("my-bucket"))).await.unwrap();
        store.insert_audit_entry(&sample_entry("GetObject", Some("my-bucket"))).await.unwrap();

        // List all
        let filter = AuditFilter { limit: 100, ..Default::default() };
        let entries = store.list_audit_entries(&filter).await.unwrap();
        assert_eq!(entries.len(), 3);

        // Count all
        let count = store.count_audit_entries(&AuditFilter::default()).await.unwrap();
        assert_eq!(count, 3);

        // Filter by bucket
        let filter = AuditFilter {
            bucket: Some("my-bucket".to_string()),
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_audit_entries(&filter).await.unwrap();
        assert_eq!(entries.len(), 2);

        // Filter by operation
        let filter = AuditFilter {
            operation: Some("PutObject".to_string()),
            limit: 100,
            ..Default::default()
        };
        let entries = store.list_audit_entries(&filter).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].operation, "PutObject");
    }

    #[tokio::test]
    async fn audit_purge() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        // Insert an entry with old timestamp
        let mut old_entry = sample_entry("GetObject", Some("old-bucket"));
        old_entry.timestamp = Utc::now() - Duration::days(100);
        store.insert_audit_entry(&old_entry).await.unwrap();

        // Insert a recent entry
        store.insert_audit_entry(&sample_entry("PutObject", Some("new-bucket"))).await.unwrap();

        // Purge entries older than 30 days
        let cutoff = Utc::now() - Duration::days(30);
        let purged = store.purge_audit_entries(cutoff).await.unwrap();
        assert_eq!(purged, 1);

        // Only the recent entry remains
        let count = store.count_audit_entries(&AuditFilter::default()).await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn audit_pagination() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        for i in 0..10 {
            let mut entry = sample_entry("GetObject", Some("bucket"));
            entry.bytes_sent = i;
            store.insert_audit_entry(&entry).await.unwrap();
        }

        // Page 1 (first 3)
        let filter = AuditFilter { limit: 3, offset: 0, ..Default::default() };
        let page1 = store.list_audit_entries(&filter).await.unwrap();
        assert_eq!(page1.len(), 3);

        // Page 2 (next 3)
        let filter = AuditFilter { limit: 3, offset: 3, ..Default::default() };
        let page2 = store.list_audit_entries(&filter).await.unwrap();
        assert_eq!(page2.len(), 3);

        // Total count
        let count = store.count_audit_entries(&AuditFilter::default()).await.unwrap();
        assert_eq!(count, 10);
    }
}
