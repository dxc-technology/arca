//! `ControlSnapshotStore` implementation for `PgStore`.
//!
//! Mirrors the sqlite implementation: build the full control snapshot (reusing
//! the list methods plus an `updated_at` lookup) and apply a merge plan via the
//! shared backend-agnostic helper.

use std::collections::HashMap;

use arca_core::cluster::{
    is_node_local_server_config_key, ControlMergePlan, ControlSnapshot, TimestampedBucketConfig,
    TimestampedBucketTags, TimestampedCredential, TimestampedServerConfig, TimestampedTeam,
    TimestampedTeamGrant, TimestampedTeamMember, TimestampedUser, TimestampedUserGrant,
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
    sql: &'static str,
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

        // R5 (TD-016) + D4 families, with their LWW timestamps; node-local
        // server_config keys never leave this node.
        let fetch =
            |sql: &'static str| sqlx_core::query::query(sql).fetch_all(&self.pool);

        let user_grants = fetch("SELECT user_id, grant_id, updated_at FROM user_grants")
            .await
            .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
            .iter()
            .map(|r| TimestampedUserGrant {
                user_id: r.get(0),
                grant_id: r.get(1),
                updated_at: r.get(2),
            })
            .collect();

        let team_grants = fetch("SELECT team_id, grant_id, updated_at FROM team_grants")
            .await
            .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
            .iter()
            .map(|r| TimestampedTeamGrant {
                team_id: r.get(0),
                grant_id: r.get(1),
                updated_at: r.get(2),
            })
            .collect();

        let team_members = fetch("SELECT team_id, user_id, updated_at FROM team_members")
            .await
            .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
            .iter()
            .map(|r| TimestampedTeamMember {
                team_id: r.get(0),
                user_id: r.get(1),
                updated_at: r.get(2),
            })
            .collect();

        let bucket_configs =
            fetch("SELECT bucket, config_key, config_value, updated_at FROM bucket_config")
                .await
                .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
                .iter()
                .map(|r| TimestampedBucketConfig {
                    bucket: r.get(0),
                    key: r.get(1),
                    value: r.get(2),
                    updated_at: r.get(3),
                })
                .collect();

        // One set-level entity per bucket: rows grouped, timestamp =
        // MAX(updated_at) (equal across the set by construction).
        let mut bucket_tags: Vec<TimestampedBucketTags> = Vec::new();
        for r in
            fetch("SELECT bucket, tag_key, tag_value, updated_at FROM bucket_tags ORDER BY bucket, tag_key")
                .await
                .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
                .iter()
        {
            let bucket: String = r.get(0);
            let pair = (r.get::<String, _>(1), r.get::<String, _>(2));
            let ts: DateTime<Utc> = r.get(3);
            match bucket_tags.last_mut() {
                Some(last) if last.bucket == bucket => {
                    last.tags.push(pair);
                    last.updated_at = last.updated_at.max(ts);
                }
                _ => bucket_tags.push(TimestampedBucketTags {
                    bucket,
                    tags: vec![pair],
                    updated_at: ts,
                }),
            }
        }

        let server_configs = fetch("SELECT config_key, config_value, updated_at FROM server_config")
            .await
            .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
            .iter()
            .map(|r| TimestampedServerConfig {
                key: r.get(0),
                value: r.get(1),
                updated_at: r.get(2),
            })
            .filter(|sc| !is_node_local_server_config_key(&sc.key))
            .collect();

        let multipart_uploads = fetch(
            "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm FROM multipart_uploads",
        )
        .await
        .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
        .iter()
        .map(super::metadata::row_to_multipart_upload_record)
        .collect();

        let parts = fetch(
            "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified FROM parts",
        )
        .await
        .map_err(|e| ArcaError::Internal(format!("build_control_snapshot: {e}")))?
        .iter()
        .map(super::metadata::row_to_part_record)
        .collect();

        Ok(ControlSnapshot {
            credentials,
            users,
            teams,
            grants: self.list_grants().await?,
            buckets: self.list_buckets().await?,
            tombstones: self.list_control_tombstones().await?,
            user_grants,
            team_grants,
            team_members,
            bucket_configs,
            bucket_tags,
            server_configs,
            multipart_uploads,
            parts,
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

    async fn apply_user_grant_at(
        &self,
        user_id: &str,
        grant_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO user_grants (user_id, grant_id, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT (user_id, grant_id) DO UPDATE SET updated_at = EXCLUDED.updated_at",
        )
        .bind(user_id)
        .bind(grant_id)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_user_grant_at: {e}")))?;
        Ok(())
    }

    async fn apply_team_grant_at(
        &self,
        team_id: &str,
        grant_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO team_grants (team_id, grant_id, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT (team_id, grant_id) DO UPDATE SET updated_at = EXCLUDED.updated_at",
        )
        .bind(team_id)
        .bind(grant_id)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_team_grant_at: {e}")))?;
        Ok(())
    }

    async fn apply_team_member_at(
        &self,
        team_id: &str,
        user_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO team_members (team_id, user_id, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT (team_id, user_id) DO UPDATE SET updated_at = EXCLUDED.updated_at",
        )
        .bind(team_id)
        .bind(user_id)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_team_member_at: {e}")))?;
        Ok(())
    }

    async fn apply_server_config_at(
        &self,
        key: &str,
        value: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        sqlx_core::query::query(
            "INSERT INTO server_config (config_key, config_value, updated_at) VALUES ($1, $2, $3)
             ON CONFLICT (config_key) DO UPDATE SET config_value = EXCLUDED.config_value, updated_at = EXCLUDED.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ArcaError::Internal(format!("apply_server_config_at: {e}")))?;
        Ok(())
    }

    async fn apply_control_merge(&self, plan: &ControlMergePlan) -> Result<(), ArcaError> {
        apply_control_merge_via_traits(self, plan).await
    }
}
