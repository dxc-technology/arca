//! SQLite implementation of the `ReplicationStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::replication::{JournalEntry, JournalFilter, ReplicationStore};
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

fn build_filter_clause(filter: &JournalFilter) -> (String, Vec<String>) {
    let mut conditions: Vec<String> = Vec::new();
    let mut vals: Vec<String> = Vec::new();
    let mut idx = 1;

    if let Some(ref bucket) = filter.bucket {
        conditions.push(format!("bucket = ?{idx}"));
        vals.push(bucket.clone());
        idx += 1;
    }
    if let Some(ref status) = filter.status {
        conditions.push(format!("status = ?{idx}"));
        vals.push(status.clone());
        idx += 1;
    }
    if let Some(ref rule) = filter.rule_id {
        conditions.push(format!("rule_id = ?{idx}"));
        vals.push(rule.clone());
        let _ = idx;
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    (where_clause, vals)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<JournalEntry> {
    Ok(JournalEntry {
        id: row.get("id")?,
        bucket: row.get("bucket")?,
        key: row.get("key")?,
        version_id: row.get("version_id")?,
        rule_id: row.get("rule_id")?,
        event_type: row.get("event_type")?,
        destination_endpoint: row.get("destination_endpoint")?,
        destination_bucket: row.get("destination_bucket")?,
        status: row.get("status")?,
        attempts: row.get::<_, u32>("attempts")?,
        last_error: row.get("last_error")?,
        next_retry_at: row
            .get::<_, String>("next_retry_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        created_at: row
            .get::<_, String>("created_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
        updated_at: row
            .get::<_, String>("updated_at")?
            .parse::<DateTime<Utc>>()
            .unwrap_or_default(),
    })
}

#[async_trait::async_trait]
impl ReplicationStore for SqliteStore {
    async fn insert_journal_entry(&self, entry: &JournalEntry) -> Result<(), ArcaError> {
        let e = entry.clone();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO replication_journal (
                        id, bucket, key, version_id, rule_id, event_type,
                        destination_endpoint, destination_bucket, status,
                        attempts, last_error, next_retry_at, created_at, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        e.id,
                        e.bucket,
                        e.key,
                        e.version_id,
                        e.rule_id,
                        e.event_type,
                        e.destination_endpoint,
                        e.destination_bucket,
                        e.status,
                        e.attempts,
                        e.last_error,
                        e.next_retry_at.to_rfc3339(),
                        e.created_at.to_rfc3339(),
                        e.updated_at.to_rfc3339(),
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("insert_journal_entry: {e}")))
    }

    async fn claim_batch(&self, limit: u32) -> Result<Vec<JournalEntry>, ArcaError> {
        let now_str = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                let tx = conn.unchecked_transaction()?;

                let ids: Vec<String> = {
                    let mut stmt = tx.prepare(
                        "SELECT id FROM replication_journal
                         WHERE status IN ('pending','failed')
                           AND next_retry_at <= ?1
                         ORDER BY next_retry_at ASC
                         LIMIT ?2",
                    )?;
                    let rows = stmt.query_map(params![now_str, limit], |row| {
                        row.get::<_, String>(0)
                    })?;
                    let mut ids = Vec::new();
                    for r in rows {
                        ids.push(r?);
                    }
                    ids
                };

                if ids.is_empty() {
                    tx.commit()?;
                    return Ok(Vec::new());
                }

                let placeholders: Vec<String> =
                    (1..=ids.len()).map(|i| format!("?{}", i + 1)).collect();
                let update_sql = format!(
                    "UPDATE replication_journal
                     SET status = 'in_flight', updated_at = ?1
                     WHERE id IN ({})",
                    placeholders.join(", ")
                );
                let mut update_params: Vec<Box<dyn rusqlite::types::ToSql>> =
                    vec![Box::new(Utc::now().to_rfc3339())];
                for id in &ids {
                    update_params.push(Box::new(id.clone()));
                }
                let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                    update_params.iter().map(|p| p.as_ref()).collect();
                tx.execute(&update_sql, param_refs.as_slice())?;

                let select_sql = format!(
                    "SELECT * FROM replication_journal WHERE id IN ({})",
                    placeholders[..ids.len()]
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("?{}", i + 1))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let mut stmt = tx.prepare(&select_sql)?;
                let id_params: Vec<&dyn rusqlite::types::ToSql> =
                    ids.iter().map(|id| id as &dyn rusqlite::types::ToSql).collect();
                let rows = stmt.query_map(id_params.as_slice(), row_to_entry)?;
                let mut entries = Vec::new();
                for r in rows {
                    entries.push(r?);
                }

                drop(stmt);
                tx.commit()?;
                Ok(entries)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("claim_batch: {e}")))
    }

    async fn update_status(
        &self,
        id: &str,
        status: &str,
        attempts: u32,
        last_error: Option<&str>,
        next_retry_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        let id = id.to_string();
        let status = status.to_string();
        let last_error = last_error.map(|s| s.to_string());
        let next_retry_str = next_retry_at.to_rfc3339();
        let now_str = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE replication_journal
                     SET status = ?1, attempts = ?2, last_error = ?3,
                         next_retry_at = ?4, updated_at = ?5
                     WHERE id = ?6",
                    params![status, attempts, last_error, next_retry_str, now_str, id],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("update_status: {e}")))
    }

    async fn list_journal(
        &self,
        filter: &JournalFilter,
    ) -> Result<Vec<JournalEntry>, ArcaError> {
        let (where_clause, params_vec) = build_filter_clause(filter);
        let limit = if filter.limit == 0 { 100 } else { filter.limit };
        let offset = filter.offset;

        self.read_conn()
            .call(move |conn| {
                let sql = format!(
                    "SELECT * FROM replication_journal {where_clause}
                     ORDER BY created_at DESC
                     LIMIT {limit} OFFSET {offset}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let refs: Vec<&dyn rusqlite::types::ToSql> = params_vec
                    .iter()
                    .map(|p| p as &dyn rusqlite::types::ToSql)
                    .collect();
                let rows = stmt.query_map(refs.as_slice(), row_to_entry)?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r?);
                }
                Ok(out)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_journal: {e}")))
    }

    async fn count_journal(&self, filter: &JournalFilter) -> Result<u64, ArcaError> {
        let (where_clause, params_vec) = build_filter_clause(filter);
        self.read_conn()
            .call(move |conn| {
                let sql = format!(
                    "SELECT COUNT(*) FROM replication_journal {where_clause}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let refs: Vec<&dyn rusqlite::types::ToSql> = params_vec
                    .iter()
                    .map(|p| p as &dyn rusqlite::types::ToSql)
                    .collect();
                let count: i64 = stmt.query_row(refs.as_slice(), |row| row.get(0))?;
                Ok(count as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("count_journal: {e}")))
    }

    async fn purge_completed(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let before_str = before.to_rfc3339();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM replication_journal
                     WHERE status = 'completed' AND updated_at < ?1",
                    params![before_str],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("purge_completed: {e}")))
    }

    async fn purge_all_older(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let before_str = before.to_rfc3339();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM replication_journal WHERE updated_at < ?1",
                    params![before_str],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("purge_all_older: {e}")))
    }

    async fn set_object_replication_status(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<(), ArcaError> {
        let bucket = bucket.to_string();
        let key = key.to_string();
        let version_id = version_id.map(|s| s.to_string());
        let status = status.map(|s| s.to_string());
        self.conn
            .call(move |conn| {
                match version_id {
                    Some(vid) => {
                        conn.execute(
                            "UPDATE objects SET replication_status = ?1
                             WHERE bucket = ?2 AND key = ?3 AND version_id = ?4",
                            params![status, bucket, key, vid],
                        )?;
                    }
                    None => {
                        conn.execute(
                            "UPDATE objects SET replication_status = ?1
                             WHERE bucket = ?2 AND key = ?3 AND version_id IS NULL AND is_latest = 1",
                            params![status, bucket, key],
                        )?;
                    }
                }
                Ok(())
            })
            .await
            .map_err(|e: TrError| {
                ArcaError::Internal(format!("set_object_replication_status: {e}"))
            })
    }
}

#[cfg(test)]
mod tests {
    use arca_core::store::replication::{JournalEntry, JournalFilter, ReplicationStore};
    use chrono::{Duration, Utc};

    use crate::sqlite::SqliteStore;

    fn sample_entry(bucket: &str, key: &str, status: &str) -> JournalEntry {
        let now = Utc::now();
        JournalEntry {
            id: uuid::Uuid::new_v4().to_string(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            version_id: Some("v1".to_string()),
            rule_id: "rule-1".to_string(),
            event_type: "put".to_string(),
            destination_endpoint: "https://replica.example.com".to_string(),
            destination_bucket: "replica".to_string(),
            status: status.to_string(),
            attempts: 0,
            last_error: None,
            next_retry_at: now,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn insert_and_list() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .insert_journal_entry(&sample_entry("b1", "k1", "pending"))
            .await
            .unwrap();
        store
            .insert_journal_entry(&sample_entry("b1", "k2", "pending"))
            .await
            .unwrap();
        store
            .insert_journal_entry(&sample_entry("b2", "k3", "completed"))
            .await
            .unwrap();

        let all = store
            .list_journal(&JournalFilter { limit: 100, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(all.len(), 3);

        let b1 = store
            .list_journal(&JournalFilter {
                bucket: Some("b1".to_string()),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(b1.len(), 2);

        let pending = store
            .list_journal(&JournalFilter {
                status: Some("pending".to_string()),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
    }

    #[tokio::test]
    async fn claim_batch_transitions_to_in_flight() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        for i in 0..3 {
            store
                .insert_journal_entry(&sample_entry("b", &format!("k{i}"), "pending"))
                .await
                .unwrap();
        }

        let claimed = store.claim_batch(10).await.unwrap();
        assert_eq!(claimed.len(), 3);
        assert!(claimed.iter().all(|e| e.status == "in_flight"));

        let pending = store
            .list_journal(&JournalFilter {
                status: Some("pending".to_string()),
                limit: 100,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn update_status_and_retry_backoff() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let entry = sample_entry("b", "k", "pending");
        let id = entry.id.clone();
        store.insert_journal_entry(&entry).await.unwrap();

        let retry = Utc::now() + Duration::seconds(30);
        store
            .update_status(&id, "failed", 1, Some("connection refused"), retry)
            .await
            .unwrap();

        let rows = store
            .list_journal(&JournalFilter {
                status: Some("failed".to_string()),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].attempts, 1);
        assert_eq!(rows[0].last_error.as_deref(), Some("connection refused"));
    }

    #[tokio::test]
    async fn purge_completed_only_drops_completed() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        let mut old_done = sample_entry("b", "done", "completed");
        old_done.updated_at = Utc::now() - Duration::days(40);
        store.insert_journal_entry(&old_done).await.unwrap();

        let mut old_failed = sample_entry("b", "failed", "failed");
        old_failed.updated_at = Utc::now() - Duration::days(40);
        store.insert_journal_entry(&old_failed).await.unwrap();

        let cutoff = Utc::now() - Duration::days(30);
        let dropped = store.purge_completed(cutoff).await.unwrap();
        assert_eq!(dropped, 1);

        let total = store
            .count_journal(&JournalFilter::default())
            .await
            .unwrap();
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn purge_all_older_is_hard_cap() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let mut old = sample_entry("b", "k", "failed");
        old.updated_at = Utc::now() - Duration::days(120);
        store.insert_journal_entry(&old).await.unwrap();

        let fresh = sample_entry("b", "k2", "pending");
        store.insert_journal_entry(&fresh).await.unwrap();

        let cutoff = Utc::now() - Duration::days(90);
        let dropped = store.purge_all_older(cutoff).await.unwrap();
        assert_eq!(dropped, 1);

        let total = store
            .count_journal(&JournalFilter::default())
            .await
            .unwrap();
        assert_eq!(total, 1);
    }
}
