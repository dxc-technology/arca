//! `ControlSnapshotStore` implementation for `PgStore`.
//!
//! Mirrors the sqlite implementation: build the full control snapshot (reusing
//! the list methods plus an `updated_at` lookup) and apply a merge plan via the
//! shared backend-agnostic helper.

use std::collections::HashMap;

use arca_core::cluster::{
    ControlMergePlan, ControlSnapshot, TimestampedCredential, TimestampedTeam, TimestampedUser,
};
use arca_core::error::ArcaError;
use arca_core::store::{
    ControlSnapshotStore, ControlTombstoneStore, CredentialStore, GrantStore, MetadataStore,
    TeamStore, UserStore,
};
use arca_core::types::{Credential, Team, User};
use chrono::{DateTime, Utc};
use sqlx_core::row::Row;

use super::PgStore;
use crate::control_merge::apply_control_merge_via_traits;

/// Reads a `primary_key -> updated_at` map from a control table.
async fn updated_at_map(
    store: &PgStore,
    sql: &str,
) -> Result<HashMap<String, DateTime<Utc>>, ArcaError> {
    let rows = sqlx_core::query::query(sql)
        .fetch_all(&store.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("updated_at_map: {e}")))?;
    Ok(rows
        .iter()
        .map(|r| {
            let key: String = r.get(0);
            let ts: DateTime<Utc> = r.get::<DateTime<Utc>, _>(1);
            (key, ts)
        })
        .collect())
}

#[async_trait::async_trait]
impl ControlSnapshotStore for PgStore {
    async fn build_control_snapshot(&self) -> Result<ControlSnapshot, ArcaError> {
        let cred_ts =
            updated_at_map(self, "SELECT access_key_id, updated_at FROM credentials").await?;
        let credentials = self
            .list_credentials()
            .await?
            .into_iter()
            .map(|c| {
                let updated_at = cred_ts.get(&c.access_key_id).copied().unwrap_or(c.created_at);
                TimestampedCredential {
                    credential: c,
                    updated_at,
                }
            })
            .collect();

        let user_ts = updated_at_map(self, "SELECT user_id, updated_at FROM users").await?;
        let users = self
            .list_users()
            .await?
            .into_iter()
            .map(|u| {
                let updated_at = user_ts.get(&u.user_id).copied().unwrap_or(u.created_at);
                TimestampedUser {
                    user: u,
                    updated_at,
                }
            })
            .collect();

        let team_ts = updated_at_map(self, "SELECT team_id, updated_at FROM teams").await?;
        let teams = self
            .list_teams()
            .await?
            .into_iter()
            .map(|t| {
                let updated_at = team_ts.get(&t.team_id).copied().unwrap_or(t.created_at);
                TimestampedTeam {
                    team: t,
                    updated_at,
                }
            })
            .collect();

        Ok(ControlSnapshot {
            credentials,
            users,
            teams,
            grants: self.list_grants().await?,
            buckets: self.list_buckets().await?,
            tombstones: self.list_control_tombstones().await?,
        })
    }

    async fn apply_credential_at(
        &self,
        credential: &Credential,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO credentials (access_key_id, secret_access_key, description, created_at, active, admin, user_id, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT (access_key_id) DO UPDATE SET
               secret_access_key = EXCLUDED.secret_access_key,
               description = EXCLUDED.description,
               created_at = EXCLUDED.created_at,
               active = EXCLUDED.active,
               admin = EXCLUDED.admin,
               user_id = EXCLUDED.user_id,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(&credential.access_key_id)
        .bind(&credential.secret_access_key)
        .bind(&credential.description)
        .bind(credential.created_at)
        .bind(credential.active)
        .bind(credential.admin)
        .bind(&credential.user_id)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_credential_at: {e}")))?;
        Ok(())
    }

    async fn apply_user_at(
        &self,
        user: &User,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO users (user_id, username, description, is_root, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (user_id) DO UPDATE SET
               username = EXCLUDED.username,
               description = EXCLUDED.description,
               is_root = EXCLUDED.is_root,
               created_at = EXCLUDED.created_at,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(&user.user_id)
        .bind(&user.username)
        .bind(&user.description)
        .bind(user.is_root)
        .bind(user.created_at)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_user_at: {e}")))?;
        Ok(())
    }

    async fn apply_team_at(
        &self,
        team: &Team,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO teams (team_id, name, description, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (team_id) DO UPDATE SET
               name = EXCLUDED.name,
               description = EXCLUDED.description,
               created_at = EXCLUDED.created_at,
               updated_at = EXCLUDED.updated_at",
        )
        .bind(&team.team_id)
        .bind(&team.name)
        .bind(&team.description)
        .bind(team.created_at)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_team_at: {e}")))?;
        Ok(())
    }

    async fn apply_control_merge(&self, plan: &ControlMergePlan) -> Result<(), ArcaError> {
        apply_control_merge_via_traits(self, plan).await
    }
}
