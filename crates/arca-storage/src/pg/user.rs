//! `UserStore` implementation for `PgStore`.

use arca_core::error::ArcaError;
use arca_core::store::UserStore;
use arca_core::types::User;
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

/// Converts a PostgreSQL row to a `User`.
pub(crate) fn row_to_user(row: &sqlx_postgres::PgRow) -> User {
    User {
        user_id: row.get("user_id"),
        username: row.get("username"),
        description: row.get("description"),
        is_root: row.get("is_root"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
    }
}

#[async_trait::async_trait]
impl UserStore for PgStore {
    async fn put_user(&self, user: &User) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO users (user_id, username, description, is_root, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&user.user_id)
        .bind(&user.username)
        .bind(&user.description)
        .bind(user.is_root)
        .bind(user.created_at)
        .bind(user.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("put_user: {e}")))?;

        Ok(())
    }

    async fn get_user(&self, user_id: &str) -> Result<Option<User>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT user_id, username, description, is_root, created_at
             FROM users WHERE user_id = $1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_user: {e}")))?;

        Ok(row.as_ref().map(row_to_user))
    }

    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT user_id, username, description, is_root, created_at
             FROM users WHERE username = $1",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_user_by_username: {e}")))?;

        Ok(row.as_ref().map(row_to_user))
    }

    async fn list_users(&self) -> Result<Vec<User>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT user_id, username, description, is_root, created_at
             FROM users ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_users: {e}")))?;

        Ok(rows.iter().map(row_to_user).collect())
    }

    async fn update_user(
        &self,
        user_id: &str,
        username: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        // Build dynamic UPDATE query.
        let mut sets = Vec::new();
        let mut param_idx = 1u32;

        if username.is_some() {
            sets.push(format!("username = ${param_idx}"));
            param_idx += 1;
        }
        if description.is_some() {
            sets.push(format!("description = ${param_idx}"));
            param_idx += 1;
        }

        if sets.is_empty() {
            // Nothing to update, just check existence.
            let row =
                sqlx_core::query::query("SELECT user_id FROM users WHERE user_id = $1")
                    .bind(user_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("update_user: {e}")))?;
            return Ok(row.is_some());
        }

        // Bump the LWW timestamp on any real change (no bound param).
        sets.push("updated_at = NOW()".to_string());
        let sql = format!(
            "UPDATE users SET {} WHERE user_id = ${param_idx}",
            sets.join(", ")
        );

        // Build query with dynamic bindings.
        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        if let Some(u) = username {
            query = query.bind(u);
        }
        if let Some(d) = description {
            query = query.bind(d);
        }
        query = query.bind(user_id);

        let result = query
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("update_user: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn delete_user(&self, user_id: &str) -> Result<bool, ArcaError> {
        // Use a transaction to remove memberships and grant attachments first.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_user: {e}")))?;

        sqlx_core::query::query("DELETE FROM team_members WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_user: {e}")))?;

        sqlx_core::query::query("DELETE FROM user_grants WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_user: {e}")))?;

        // Cascade credential deletion: a credential must never outlive its
        // user, otherwise it would authenticate to a non-existent identity
        // (which the auth layer now denies).
        sqlx_core::query::query("DELETE FROM credentials WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_user: {e}")))?;

        let result = sqlx_core::query::query("DELETE FROM users WHERE user_id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_user: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_user: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn apply_remote_user(&self, user: &User) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO users (user_id, username, description, is_root, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, NOW())
             ON CONFLICT (user_id) DO UPDATE SET
               username = EXCLUDED.username,
               description = EXCLUDED.description,
               is_root = EXCLUDED.is_root,
               created_at = EXCLUDED.created_at,
               updated_at = NOW()",
        )
        .bind(&user.user_id)
        .bind(&user.username)
        .bind(&user.description)
        .bind(user.is_root)
        .bind(user.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_remote_user: {e}")))?;
        Ok(())
    }
}
