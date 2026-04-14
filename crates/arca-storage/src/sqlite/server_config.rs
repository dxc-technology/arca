//! SQLite implementation of the `ServerConfigStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::server_config::ServerConfigStore;
use rusqlite::params;

use super::{SqliteStore, TrError};

#[async_trait::async_trait]
impl ServerConfigStore for SqliteStore {
    async fn get_server_config(&self, key: &str) -> Result<Option<String>, ArcaError> {
        let key = key.to_string();
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT config_value FROM server_config WHERE config_key = ?1",
                )?;
                let result = stmt.query_row(params![key], |row| row.get(0));
                match result {
                    Ok(val) => Ok(Some(val)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(e.into()),
                }
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("get_server_config: {e}")))
    }

    async fn set_server_config(&self, key: &str, value: &str) -> Result<(), ArcaError> {
        let key = key.to_string();
        let value = value.to_string();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO server_config (config_key, config_value, updated_at)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(config_key) DO UPDATE SET config_value = ?2, updated_at = ?3",
                    params![key, value, chrono::Utc::now().to_rfc3339()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("set_server_config: {e}")))
    }

    async fn delete_server_config(&self, key: &str) -> Result<bool, ArcaError> {
        let key = key.to_string();
        self.conn
            .call(move |conn| {
                let affected = conn.execute(
                    "DELETE FROM server_config WHERE config_key = ?1",
                    params![key],
                )?;
                Ok(affected > 0)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("delete_server_config: {e}")))
    }

    async fn list_server_config(&self) -> Result<Vec<(String, String)>, ArcaError> {
        self.read_conn()
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT config_key, config_value FROM server_config ORDER BY config_key",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                let mut result = Vec::new();
                for row in rows {
                    result.push(row?);
                }
                Ok(result)
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("list_server_config: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use arca_core::store::server_config::ServerConfigStore;

    use crate::sqlite::SqliteStore;

    #[tokio::test]
    async fn server_config_crud() {
        let store = SqliteStore::open_in_memory().await.unwrap();

        // Initially empty
        assert!(store.get_server_config("region").await.unwrap().is_none());
        assert!(store.list_server_config().await.unwrap().is_empty());

        // Set a value
        store.set_server_config("region", "eu-west-1").await.unwrap();
        assert_eq!(
            store.get_server_config("region").await.unwrap().as_deref(),
            Some("eu-west-1")
        );

        // Update the value
        store.set_server_config("region", "us-west-2").await.unwrap();
        assert_eq!(
            store.get_server_config("region").await.unwrap().as_deref(),
            Some("us-west-2")
        );

        // Set another key
        store.set_server_config("audit_retention_days", "90").await.unwrap();

        // List all
        let all = store.list_server_config().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], ("audit_retention_days".to_string(), "90".to_string()));
        assert_eq!(all[1], ("region".to_string(), "us-west-2".to_string()));

        // Delete
        assert!(store.delete_server_config("region").await.unwrap());
        assert!(!store.delete_server_config("region").await.unwrap()); // already gone
        assert!(store.get_server_config("region").await.unwrap().is_none());
    }
}
