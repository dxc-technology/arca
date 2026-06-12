//! `TeamStore` implementation for `PgStore`.

use arca_core::error::ArcaError;
use arca_core::store::TeamStore;
use arca_core::types::{Team, User};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

/// Converts a PostgreSQL row to a `Team`.
fn row_to_team(row: &sqlx_postgres::PgRow) -> Team {
    Team {
        team_id: row.get("team_id"),
        name: row.get("name"),
        description: row.get("description"),
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
    }
}

#[async_trait::async_trait]
impl TeamStore for PgStore {
    async fn put_team(&self, team: &Team) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO teams (team_id, name, description, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(&team.team_id)
        .bind(&team.name)
        .bind(&team.description)
        .bind(team.created_at)
        .bind(team.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("put_team: {e}")))?;

        Ok(())
    }

    async fn get_team(&self, team_id: &str) -> Result<Option<Team>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT team_id, name, description, created_at
             FROM teams WHERE team_id = $1",
        )
        .bind(team_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_team: {e}")))?;

        Ok(row.as_ref().map(row_to_team))
    }

    async fn list_teams(&self) -> Result<Vec<Team>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT team_id, name, description, created_at
             FROM teams ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_teams: {e}")))?;

        Ok(rows.iter().map(row_to_team).collect())
    }

    async fn update_team(
        &self,
        team_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        // Build dynamic UPDATE query.
        let mut sets = Vec::new();
        let mut param_idx = 1u32;

        if name.is_some() {
            sets.push(format!("name = ${param_idx}"));
            param_idx += 1;
        }
        if description.is_some() {
            sets.push(format!("description = ${param_idx}"));
            param_idx += 1;
        }

        if sets.is_empty() {
            // Nothing to update, just check existence.
            let row =
                sqlx_core::query::query("SELECT team_id FROM teams WHERE team_id = $1")
                    .bind(team_id)
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("update_team: {e}")))?;
            return Ok(row.is_some());
        }

        // Bump the LWW timestamp on any real change (no bound param).
        sets.push("updated_at = NOW()".to_string());
        let sql = format!(
            "UPDATE teams SET {} WHERE team_id = ${param_idx}",
            sets.join(", ")
        );

        // Build query with dynamic bindings.
        let mut query = sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(sql.as_str()));
        if let Some(n) = name {
            query = query.bind(n);
        }
        if let Some(d) = description {
            query = query.bind(d);
        }
        query = query.bind(team_id);

        let result = query
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("update_team: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn delete_team(&self, team_id: &str) -> Result<bool, ArcaError> {
        // Use a transaction to remove memberships and grant attachments first.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_team: {e}")))?;

        sqlx_core::query::query("DELETE FROM team_members WHERE team_id = $1")
            .bind(team_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_team: {e}")))?;

        sqlx_core::query::query("DELETE FROM team_grants WHERE team_id = $1")
            .bind(team_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_team: {e}")))?;

        let result = sqlx_core::query::query("DELETE FROM teams WHERE team_id = $1")
            .bind(team_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_team: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_team: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn add_member(&self, team_id: &str, user_id: &str) -> Result<(), ArcaError> {
        // updated_at refreshed on an idempotent re-add too: the LWW reconcile
        // (R5) must see it as newer than any concurrent remove-member tombstone.
        sqlx_core::query::query(
            "INSERT INTO team_members (team_id, user_id, updated_at)
             VALUES ($1, $2, NOW())
             ON CONFLICT (team_id, user_id) DO UPDATE SET updated_at = EXCLUDED.updated_at",
        )
        .bind(team_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("add_member: {e}")))?;

        Ok(())
    }

    async fn remove_member(&self, team_id: &str, user_id: &str) -> Result<bool, ArcaError> {
        let result =
            sqlx_core::query::query("DELETE FROM team_members WHERE team_id = $1 AND user_id = $2")
                .bind(team_id)
                .bind(user_id)
                .execute(&self.pool)
                .await
                .map_err(|e| ArcaError::Internal(format!("remove_member: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn list_members(&self, team_id: &str) -> Result<Vec<User>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT u.user_id, u.username, u.description, u.is_root, u.created_at
             FROM users u
             JOIN team_members tm ON tm.user_id = u.user_id
             WHERE tm.team_id = $1
             ORDER BY u.username",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_members: {e}")))?;

        Ok(rows.iter().map(super::user::row_to_user).collect())
    }

    async fn list_user_teams(&self, user_id: &str) -> Result<Vec<Team>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT t.team_id, t.name, t.description, t.created_at
             FROM teams t
             JOIN team_members tm ON tm.team_id = t.team_id
             WHERE tm.user_id = $1
             ORDER BY t.name",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_user_teams: {e}")))?;

        Ok(rows.iter().map(row_to_team).collect())
    }

    async fn apply_remote_team(&self, team: &Team) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO teams (team_id, name, description, created_at, updated_at)
             VALUES ($1, $2, $3, $4, NOW())
             ON CONFLICT (team_id) DO UPDATE SET
               name = EXCLUDED.name,
               description = EXCLUDED.description,
               created_at = EXCLUDED.created_at,
               updated_at = NOW()",
        )
        .bind(&team.team_id)
        .bind(&team.name)
        .bind(&team.description)
        .bind(team.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_remote_team: {e}")))?;
        Ok(())
    }
}
