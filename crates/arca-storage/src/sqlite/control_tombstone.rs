//! `ControlTombstoneStore` implementation for `SqliteStore`.

use arca_core::error::ArcaError;
use arca_core::store::{ControlTombstone, ControlTombstoneStore};
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl ControlTombstoneStore for SqliteStore {
    async fn record_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<(), ArcaError> {
        let etype = entity_type.to_string();
        let ekey = entity_key.to_string();
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO control_tombstones (entity_type, entity_key, deleted_at)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(entity_type, entity_key) DO UPDATE SET
                       deleted_at = excluded.deleted_at",
                    params![etype, ekey, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("record_control_tombstone: {e}")))
    }

    async fn list_control_tombstones(&self) -> Result<Vec<ControlTombstone>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT entity_type, entity_key, deleted_at FROM control_tombstones",
                )?;
                let rows = stmt.query_map([], |row| Ok(row_to_tombstone(row)))?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row??);
                }
                Ok(out)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_control_tombstones: {e}")))
    }

    async fn delete_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<bool, ArcaError> {
        let etype = entity_type.to_string();
        let ekey = entity_key.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM control_tombstones WHERE entity_type = ?1 AND entity_key = ?2",
                    params![etype, ekey],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_control_tombstone: {e}")))
    }

    async fn purge_control_tombstones(&self, before: DateTime<Utc>) -> Result<u64, ArcaError> {
        let cutoff = before.to_rfc3339();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM control_tombstones WHERE deleted_at < ?1",
                    params![cutoff],
                )?;
                Ok(affected as u64)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("purge_control_tombstones: {e}")))
    }
}

/// Converts a SQLite row to a `ControlTombstone`.
///
/// Expects columns: entity_type, entity_key, deleted_at.
fn row_to_tombstone(row: &rusqlite::Row) -> Result<ControlTombstone, rusqlite::Error> {
    let deleted_at_str: String = row.get(2)?;
    let deleted_at = DateTime::parse_from_rfc3339(&deleted_at_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(ControlTombstone {
        entity_type: row.get(0)?,
        entity_key: row.get(1)?,
        deleted_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::TOMBSTONE_CREDENTIAL;

    async fn test_store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    #[tokio::test]
    async fn record_list_delete_roundtrip() {
        let store = test_store().await;
        assert!(store.list_control_tombstones().await.unwrap().is_empty());

        store
            .record_control_tombstone(TOMBSTONE_CREDENTIAL, "AKIA1")
            .await
            .unwrap();
        let list = store.list_control_tombstones().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].entity_type, TOMBSTONE_CREDENTIAL);
        assert_eq!(list[0].entity_key, "AKIA1");

        // Idempotent: recording the same key again does not duplicate.
        store
            .record_control_tombstone(TOMBSTONE_CREDENTIAL, "AKIA1")
            .await
            .unwrap();
        assert_eq!(store.list_control_tombstones().await.unwrap().len(), 1);

        // Delete clears it (re-create path).
        assert!(store
            .delete_control_tombstone(TOMBSTONE_CREDENTIAL, "AKIA1")
            .await
            .unwrap());
        assert!(store.list_control_tombstones().await.unwrap().is_empty());
        // Deleting a missing tombstone returns false.
        assert!(!store
            .delete_control_tombstone(TOMBSTONE_CREDENTIAL, "AKIA1")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn purge_removes_only_old_tombstones() {
        let store = test_store().await;
        store
            .record_control_tombstone(TOMBSTONE_CREDENTIAL, "old")
            .await
            .unwrap();

        // Cutoff in the future: removes the just-recorded (now-stamped) tombstone.
        let removed = store
            .purge_control_tombstones(Utc::now() + chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert!(store.list_control_tombstones().await.unwrap().is_empty());

        // A fresh tombstone is NOT removed by a past cutoff.
        store
            .record_control_tombstone(TOMBSTONE_CREDENTIAL, "fresh")
            .await
            .unwrap();
        let removed = store
            .purge_control_tombstones(Utc::now() - chrono::Duration::hours(1))
            .await
            .unwrap();
        assert_eq!(removed, 0);
        assert_eq!(store.list_control_tombstones().await.unwrap().len(), 1);
    }
}
