//! PostgreSQL implementation of the `CredentialStore` trait.

use arca_core::error::ArcaError;
use arca_core::store::CredentialStore;
use arca_core::types::Credential;
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

#[async_trait::async_trait]
impl CredentialStore for PgStore {
    async fn put_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO credentials (access_key_id, secret_access_key, description, created_at, active, admin, user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&credential.access_key_id)
        .bind(&credential.secret_access_key)
        .bind(&credential.description)
        .bind(credential.created_at)
        .bind(credential.active)
        .bind(credential.admin)
        .bind(&credential.user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("put_credential: {e}")))?;

        Ok(())
    }

    async fn get_credential(
        &self,
        access_key_id: &str,
    ) -> Result<Option<Credential>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT access_key_id, secret_access_key, description, created_at, active, admin, user_id
             FROM credentials WHERE access_key_id = $1",
        )
        .bind(access_key_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_credential: {e}")))?;

        Ok(row.as_ref().map(row_to_credential))
    }

    async fn list_credentials(&self) -> Result<Vec<Credential>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT access_key_id, secret_access_key, description, created_at, active, admin, user_id
             FROM credentials ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_credentials: {e}")))?;

        Ok(rows.iter().map(row_to_credential).collect())
    }

    async fn delete_credential(&self, access_key_id: &str) -> Result<bool, ArcaError> {
        let result = sqlx_core::query::query("DELETE FROM credentials WHERE access_key_id = $1")
            .bind(access_key_id)
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_credential: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn update_credential(
        &self,
        access_key_id: &str,
        active: Option<bool>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        // Build dynamic UPDATE query.
        let mut sets = Vec::new();
        let mut param_idx = 1u32;

        if active.is_some() {
            sets.push(format!("active = ${param_idx}"));
            param_idx += 1;
        }
        if description.is_some() {
            sets.push(format!("description = ${param_idx}"));
            param_idx += 1;
        }

        if sets.is_empty() {
            // Nothing to update, just check existence.
            let row = sqlx_core::query::query(
                "SELECT access_key_id FROM credentials WHERE access_key_id = $1",
            )
            .bind(access_key_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("update_credential: {e}")))?;
            return Ok(row.is_some());
        }

        let sql = format!(
            "UPDATE credentials SET {} WHERE access_key_id = ${param_idx}",
            sets.join(", ")
        );

        // Build query with dynamic bindings.
        let mut query = sqlx_core::query::query(&sql);
        if let Some(a) = active {
            query = query.bind(a);
        }
        if let Some(d) = description {
            query = query.bind(d);
        }
        query = query.bind(access_key_id);

        let result = query
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("update_credential: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn count_active_credentials(&self) -> Result<u64, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT COUNT(*) FROM credentials WHERE active = TRUE",
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("count_active_credentials: {e}")))?;

        let count: i64 = row.get(0);
        Ok(count as u64)
    }

    async fn apply_remote_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO credentials (access_key_id, secret_access_key, description, created_at, active, admin, user_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (access_key_id) DO UPDATE SET
               secret_access_key = EXCLUDED.secret_access_key,
               description = EXCLUDED.description,
               created_at = EXCLUDED.created_at,
               active = EXCLUDED.active,
               admin = EXCLUDED.admin,
               user_id = EXCLUDED.user_id",
        )
        .bind(&credential.access_key_id)
        .bind(&credential.secret_access_key)
        .bind(&credential.description)
        .bind(credential.created_at)
        .bind(credential.active)
        .bind(credential.admin)
        .bind(&credential.user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_remote_credential: {e}")))?;
        Ok(())
    }
}

/// Converts a PostgreSQL row to a `Credential`.
fn row_to_credential(row: &sqlx_postgres::PgRow) -> Credential {
    Credential {
        access_key_id: row.get("access_key_id"),
        secret_access_key: row.get("secret_access_key"),
        description: row.get("description"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        active: row.get("active"),
        admin: row.get("admin"),
        user_id: row.get("user_id"),
    }
}
