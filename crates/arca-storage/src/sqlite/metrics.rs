//! SQLite implementation of the `MetricsStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::metrics::{MetricsSnapshot, MetricsStore};
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

fn row_to_metrics_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<MetricsSnapshot> {
    Ok(MetricsSnapshot {
        id: row.get("id")?,
        timestamp: row
            .get::<_, String>("timestamp")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        bucket_count: row.get::<_, i64>("bucket_count")? as u64,
        object_count: row.get::<_, i64>("object_count")? as u64,
        total_size_bytes: row.get::<_, i64>("total_size_bytes")? as u64,
        disk_total_bytes: row
            .get::<_, Option<i64>>("disk_total_bytes")?
            .map(|v| v as u64),
        disk_available_bytes: row
            .get::<_, Option<i64>>("disk_available_bytes")?
            .map(|v| v as u64),
        active_connections: row.get::<_, i64>("active_connections")? as u64,
    })
}

#[async_trait::async_trait]
impl MetricsStore for SqliteStore {
    async fn insert_metrics_snapshot(
        &self,
        snapshot: &MetricsSnapshot,
    ) -> Result<(), ArcaError> {
        let snapshot = snapshot.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO metrics_snapshot (timestamp, bucket_count, object_count,
                        total_size_bytes, disk_total_bytes, disk_available_bytes,
                        active_connections)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        snapshot.timestamp.to_rfc3339(),
                        snapshot.bucket_count as i64,
                        snapshot.object_count as i64,
                        snapshot.total_size_bytes as i64,
                        snapshot.disk_total_bytes.map(|v| v as i64),
                        snapshot.disk_available_bytes.map(|v| v as i64),
                        snapshot.active_connections as i64,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("insert_metrics_snapshot: {e}")))
    }

    async fn list_metrics_snapshots(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: u32,
    ) -> Result<Vec<MetricsSnapshot>, ArcaError> {
        let limit = if limit == 0 { 500 } else { limit };

        self.read_conn()
            .call(move |conn| {
                let mut conditions: Vec<String> = Vec::new();
                let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                let mut idx = 1;

                if let Some(ref from) = from {
                    conditions.push(format!("timestamp >= ?{idx}"));
                    params_vec.push(Box::new(from.to_rfc3339()));
                    idx += 1;
                }
                if let Some(ref to) = to {
                    conditions.push(format!("timestamp <= ?{idx}"));
                    params_vec.push(Box::new(to.to_rfc3339()));
                    let _ = idx;
                }

                let where_clause = if conditions.is_empty() {
                    String::new()
                } else {
                    format!("WHERE {}", conditions.join(" AND "))
                };

                let params_refs: Vec<&dyn rusqlite::types::ToSql> =
                    params_vec.iter().map(|p| p.as_ref()).collect();

                // Count total matching rows to decide if downsampling is needed
                let count_sql =
                    format!("SELECT COUNT(*) FROM metrics_snapshot {where_clause}");
                let total: u64 =
                    conn.query_row(&count_sql, params_refs.as_slice(), |row| row.get(0))?;

                let limit_u64 = limit as u64;

                let sql = if total <= limit_u64 {
                    // Few enough rows: return all, no sampling needed
                    format!(
                        "SELECT * FROM metrics_snapshot {where_clause} ORDER BY timestamp DESC"
                    )
                } else {
                    // Downsample: use ROW_NUMBER to pick evenly spaced points
                    let step = total / limit_u64;
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

                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params_refs.as_slice(), row_to_metrics_snapshot)?;
                let mut snapshots = Vec::new();
                for row in rows {
                    snapshots.push(row?);
                }
                Ok(snapshots)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_metrics_snapshots: {e}")))
    }

    async fn purge_metrics_snapshots(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, ArcaError> {
        let before_str = before.to_rfc3339();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM metrics_snapshot WHERE timestamp < ?1",
                    params![before_str],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("purge_metrics_snapshots: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use arca_core::store::metrics::{MetricsSnapshot, MetricsStore};
    use chrono::{Duration, Utc};

    use crate::sqlite::SqliteStore;

    fn sample_snapshot() -> MetricsSnapshot {
        MetricsSnapshot {
            id: 0,
            timestamp: Utc::now(),
            bucket_count: 5,
            object_count: 1000,
            total_size_bytes: 1_073_741_824,
            disk_total_bytes: Some(10_737_418_240),
            disk_available_bytes: Some(5_368_709_120),
            active_connections: 3,
        }
    }

    #[tokio::test]
    async fn metrics_insert_and_list() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        store.insert_metrics_snapshot(&sample_snapshot()).await.unwrap();
        store.insert_metrics_snapshot(&sample_snapshot()).await.unwrap();

        let snapshots = store.list_metrics_snapshots(None, None, 100).await.unwrap();
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].bucket_count, 5);
        assert_eq!(snapshots[0].total_size_bytes, 1_073_741_824);
    }

    #[tokio::test]
    async fn metrics_purge() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        // Insert old snapshot
        let mut old = sample_snapshot();
        old.timestamp = Utc::now() - Duration::days(60);
        store.insert_metrics_snapshot(&old).await.unwrap();

        // Insert recent snapshot
        store.insert_metrics_snapshot(&sample_snapshot()).await.unwrap();

        // Purge older than 30 days
        let cutoff = Utc::now() - Duration::days(30);
        let purged = store.purge_metrics_snapshots(cutoff).await.unwrap();
        assert_eq!(purged, 1);

        let remaining = store.list_metrics_snapshots(None, None, 100).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn metrics_time_range_filter() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        // Insert snapshots at different times
        let mut s1 = sample_snapshot();
        s1.timestamp = Utc::now() - Duration::hours(2);
        s1.bucket_count = 3;
        store.insert_metrics_snapshot(&s1).await.unwrap();

        let mut s2 = sample_snapshot();
        s2.timestamp = Utc::now() - Duration::hours(1);
        s2.bucket_count = 4;
        store.insert_metrics_snapshot(&s2).await.unwrap();

        let s3 = sample_snapshot(); // now
        store.insert_metrics_snapshot(&s3).await.unwrap();

        // Filter: only last 90 minutes
        let from = Utc::now() - Duration::minutes(90);
        let snapshots = store.list_metrics_snapshots(Some(from), None, 100).await.unwrap();
        assert_eq!(snapshots.len(), 2);
    }
}
