//! `GrantStore` implementation for `PgStore`.

use arca_core::error::ArcaError;
use arca_core::policy::PolicyDocument;
use arca_core::store::GrantStore;
use arca_core::types::Grant;
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;

/// Converts a PostgreSQL row to a `Grant`.
fn row_to_grant(row: &sqlx_postgres::PgRow) -> Result<Grant, ArcaError> {
    let document_json: serde_json::Value = row.get("document");
    let document: PolicyDocument =
        serde_json::from_value(document_json).map_err(|e| {
            ArcaError::Internal(format!("deserialize grant document: {e}"))
        })?;
    Ok(Grant {
        grant_id: row.get("grant_id"),
        name: row.get("name"),
        description: row.get("description"),
        document,
        created_at: row.get::<DateTime<Utc>, _>("created_at"),
        updated_at: row.get::<DateTime<Utc>, _>("updated_at"),
    })
}

#[async_trait::async_trait]
impl GrantStore for PgStore {
    async fn put_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        let doc_json = serde_json::to_value(&grant.document)
            .map_err(|e| ArcaError::Internal(format!("serialize grant document: {e}")))?;

        sqlx_core::query::query(
            "INSERT INTO grants (grant_id, name, description, document, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&grant.grant_id)
        .bind(&grant.name)
        .bind(&grant.description)
        .bind(&doc_json)
        .bind(grant.created_at)
        .bind(grant.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("put_grant: {e}")))?;

        Ok(())
    }

    async fn get_grant(&self, grant_id: &str) -> Result<Option<Grant>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT grant_id, name, description, document, created_at, updated_at
             FROM grants WHERE grant_id = $1",
        )
        .bind(grant_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_grant: {e}")))?;

        row.map(|r| row_to_grant(&r)).transpose()
    }

    async fn get_grant_by_name(&self, name: &str) -> Result<Option<Grant>, ArcaError> {
        let row = sqlx_core::query::query(
            "SELECT grant_id, name, description, document, created_at, updated_at
             FROM grants WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_grant_by_name: {e}")))?;

        row.map(|r| row_to_grant(&r)).transpose()
    }

    async fn list_grants(&self) -> Result<Vec<Grant>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT grant_id, name, description, document, created_at, updated_at
             FROM grants ORDER BY created_at",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_grants: {e}")))?;

        rows.iter().map(row_to_grant).collect()
    }

    async fn update_grant(
        &self,
        grant_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        document: Option<&PolicyDocument>,
    ) -> Result<bool, ArcaError> {
        let doc_json = document
            .map(serde_json::to_value)
            .transpose()
            .map_err(|e| ArcaError::Internal(format!("serialize grant document: {e}")))?;

        // Build dynamic UPDATE query. updated_at is always set.
        let mut sets = vec!["updated_at = NOW()".to_string()];
        let mut param_idx = 1u32;

        if name.is_some() {
            sets.push(format!("name = ${param_idx}"));
            param_idx += 1;
        }
        if description.is_some() {
            sets.push(format!("description = ${param_idx}"));
            param_idx += 1;
        }
        if doc_json.is_some() {
            sets.push(format!("document = ${param_idx}"));
            param_idx += 1;
        }

        let sql = format!(
            "UPDATE grants SET {} WHERE grant_id = ${param_idx}",
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
        if let Some(ref dj) = doc_json {
            query = query.bind(dj);
        }
        query = query.bind(grant_id);

        let result = query
            .execute(&self.pool)
            .await
            .map_err(|e| ArcaError::Internal(format!("update_grant: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn delete_grant(&self, grant_id: &str) -> Result<bool, ArcaError> {
        // Use a transaction to remove attachments first.
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_grant: {e}")))?;

        sqlx_core::query::query("DELETE FROM user_grants WHERE grant_id = $1")
            .bind(grant_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_grant: {e}")))?;

        sqlx_core::query::query("DELETE FROM team_grants WHERE grant_id = $1")
            .bind(grant_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_grant: {e}")))?;

        let result = sqlx_core::query::query("DELETE FROM grants WHERE grant_id = $1")
            .bind(grant_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_grant: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| ArcaError::Internal(format!("delete_grant: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn attach_to_user(&self, user_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        // Refresh updated_at on an idempotent re-attach too: the LWW reconcile
        // (R5) must see a re-attach as newer than any concurrent detach
        // tombstone, or the user's intent is lost.
        sqlx_core::query::query(
            "INSERT INTO user_grants (user_id, grant_id, updated_at)
             VALUES ($1, $2, NOW())
             ON CONFLICT (user_id, grant_id) DO UPDATE SET updated_at = EXCLUDED.updated_at",
        )
        .bind(user_id)
        .bind(grant_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("attach_to_user: {e}")))?;

        Ok(())
    }

    async fn detach_from_user(&self, user_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        let result =
            sqlx_core::query::query("DELETE FROM user_grants WHERE user_id = $1 AND grant_id = $2")
                .bind(user_id)
                .bind(grant_id)
                .execute(&self.pool)
                .await
                .map_err(|e| ArcaError::Internal(format!("detach_from_user: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn attach_to_team(&self, team_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        // updated_at refreshed on re-attach — see attach_to_user.
        sqlx_core::query::query(
            "INSERT INTO team_grants (team_id, grant_id, updated_at)
             VALUES ($1, $2, NOW())
             ON CONFLICT (team_id, grant_id) DO UPDATE SET updated_at = EXCLUDED.updated_at",
        )
        .bind(team_id)
        .bind(grant_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("attach_to_team: {e}")))?;

        Ok(())
    }

    async fn detach_from_team(&self, team_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        let result =
            sqlx_core::query::query("DELETE FROM team_grants WHERE team_id = $1 AND grant_id = $2")
                .bind(team_id)
                .bind(grant_id)
                .execute(&self.pool)
                .await
                .map_err(|e| ArcaError::Internal(format!("detach_from_team: {e}")))?;

        Ok(result.rows_affected() > 0)
    }

    async fn list_user_grants(&self, user_id: &str) -> Result<Vec<Grant>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT g.grant_id, g.name, g.description, g.document, g.created_at, g.updated_at
             FROM grants g
             JOIN user_grants ug ON ug.grant_id = g.grant_id
             WHERE ug.user_id = $1
             ORDER BY g.name",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_user_grants: {e}")))?;

        rows.iter().map(row_to_grant).collect()
    }

    async fn list_team_grants(&self, team_id: &str) -> Result<Vec<Grant>, ArcaError> {
        let rows = sqlx_core::query::query(
            "SELECT g.grant_id, g.name, g.description, g.document, g.created_at, g.updated_at
             FROM grants g
             JOIN team_grants tg ON tg.grant_id = g.grant_id
             WHERE tg.team_id = $1
             ORDER BY g.name",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("list_team_grants: {e}")))?;

        rows.iter().map(row_to_grant).collect()
    }

    async fn get_effective_policies(
        &self,
        user_id: &str,
    ) -> Result<Vec<PolicyDocument>, ArcaError> {
        // Fetch all distinct policy documents for this user: direct grants + team grants.
        let rows = sqlx_core::query::query(
            "SELECT g.document
             FROM grants g
             WHERE g.grant_id IN (
                 SELECT grant_id FROM user_grants WHERE user_id = $1
                 UNION
                 SELECT tg.grant_id FROM team_grants tg
                 JOIN team_members tm ON tm.team_id = tg.team_id
                 WHERE tm.user_id = $1
             )",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("get_effective_policies: {e}")))?;

        let mut policies = Vec::with_capacity(rows.len());
        for row in &rows {
            let doc_value: serde_json::Value = row.get("document");
            let doc: PolicyDocument = serde_json::from_value(doc_value).map_err(|e| {
                ArcaError::Internal(format!("deserialize effective policy: {e}"))
            })?;
            policies.push(doc);
        }
        Ok(policies)
    }

    async fn apply_remote_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        let doc_json = serde_json::to_value(&grant.document)
            .map_err(|e| ArcaError::Internal(format!("serialize grant document: {e}")))?;
        sqlx_core::query::query(
            "INSERT INTO grants (grant_id, name, description, document, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (grant_id) DO UPDATE SET
               name = EXCLUDED.name,
               description = EXCLUDED.description,
               document = EXCLUDED.document,
               created_at = EXCLUDED.created_at,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(&grant.grant_id)
        .bind(&grant.name)
        .bind(&grant.description)
        .bind(&doc_json)
        .bind(grant.created_at)
        .bind(grant.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_remote_grant: {e}")))?;
        Ok(())
    }
}
