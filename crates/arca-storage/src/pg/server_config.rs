//! PostgreSQL implementation of the `ServerConfigStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::server_config::ServerConfigStore;
use sqlx_core::row::Row;

use super::PgStore;

#[async_trait::async_trait]
impl ServerConfigStore for PgStore {
    async fn get_server_config(&self, key: &str) -> Result<Option<String>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT config_value FROM server_config WHERE config_key = $1",
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_server_config: {e}")))?;

        Ok(row.map(|r| r.get("config_value")))
    }

    async fn set_server_config(&self, key: &str, value: &str) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO server_config (config_key, config_value, updated_at)
             VALUES ($1, $2, NOW())
             ON CONFLICT (config_key) DO UPDATE SET config_value = $2, updated_at = NOW()",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("set_server_config: {e}")))?;

        Ok(())
    }

    async fn delete_server_config(&self, key: &str) -> Result<bool, ArcaError> {
        let result = sqlx_core::query::query("DELETE FROM server_config WHERE config_key = $1")
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_server_config: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn list_server_config(&self) -> Result<Vec<(String, String)>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT config_key, config_value FROM server_config ORDER BY config_key",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_server_config: {e}")))?;

        Ok(rows.iter().map(|r| (r.get("config_key"), r.get("config_value"))).collect())
    }
}
