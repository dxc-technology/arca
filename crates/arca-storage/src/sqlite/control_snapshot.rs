//! `ControlSnapshotStore` implementation for `SqliteStore`.
//!
//! Builds this node's full control snapshot and applies a computed merge plan.
//! `build_control_snapshot` reuses the existing list methods for the entity
//! payloads and reads the `updated_at` column separately (it is not carried on
//! the in-memory structs). `apply_control_merge` is mechanical: it delegates to
//! the per-entity store methods (`apply_*_at`, `apply_remote_grant`,
//! `apply_remote_bucket`, `delete_*`, tombstone CRUD).

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

use crate::control_merge::apply_control_merge_via_traits;
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::{SqliteStore, TrError};

/// Reads a `primary_key -> updated_at` map from a control table.
async fn updated_at_map(
    store: &SqliteStore,
    sql: &'static str,
) -> Result<HashMap<String, DateTime<Utc>>, ArcaError> {
    store
        .read_conn()
        .call(move |conn| {
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |row| {
                let key: String = row.get(0)?;
                let ts: String = row.get(1)?;
                Ok((key, ts))
            })?;
            let mut out: HashMap<String, DateTime<Utc>> = HashMap::new();
            for row in rows {
                let (key, ts) = row?;
                let parsed = DateTime::parse_from_rfc3339(&ts)
                    .map(|dt| dt.with_timezone(&Utc))
                    .map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?;
                out.insert(key, parsed);
            }
            Ok(out)
        })
        .await
        .map_err(|e: TrError| ArcaError::Internal(format!("updated_at_map: {e}")))
}

#[async_trait::async_trait]
impl ControlSnapshotStore for SqliteStore {
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
        let cred = credential.clone();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO credentials (access_key_id, secret_access_key, description, created_at, active, admin, user_id, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(access_key_id) DO UPDATE SET
                       secret_access_key = excluded.secret_access_key,
                       description = excluded.description,
                       created_at = excluded.created_at,
                       active = excluded.active,
                       admin = excluded.admin,
                       user_id = excluded.user_id,
                       updated_at = excluded.updated_at",
                    params![
                        cred.access_key_id,
                        cred.secret_access_key,
                        cred.description,
                        cred.created_at.to_rfc3339(),
                        cred.active as i32,
                        cred.admin as i32,
                        cred.user_id,
                        ts,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_credential_at: {e}")))
    }

    async fn apply_user_at(
        &self,
        user: &User,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        let u = user.clone();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO users (user_id, username, description, is_root, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(user_id) DO UPDATE SET
                       username = excluded.username,
                       description = excluded.description,
                       is_root = excluded.is_root,
                       created_at = excluded.created_at,
                       updated_at = excluded.updated_at",
                    params![
                        u.user_id,
                        u.username,
                        u.description,
                        u.is_root as i32,
                        u.created_at.to_rfc3339(),
                        ts,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_user_at: {e}")))
    }

    async fn apply_team_at(
        &self,
        team: &Team,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        let t = team.clone();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO teams (team_id, name, description, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(team_id) DO UPDATE SET
                       name = excluded.name,
                       description = excluded.description,
                       created_at = excluded.created_at,
                       updated_at = excluded.updated_at",
                    params![
                        t.team_id,
                        t.name,
                        t.description,
                        t.created_at.to_rfc3339(),
                        ts,
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_team_at: {e}")))
    }

    async fn apply_control_merge(&self, plan: &ControlMergePlan) -> Result<(), ArcaError> {
        apply_control_merge_via_traits(self, plan).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::cluster::plan_control_merge;
    use arca_core::types::Credential;

    async fn store() -> SqliteStore {
        SqliteStore::open_in_memory().await.unwrap()
    }

    fn cred(id: &str) -> Credential {
        Credential {
            access_key_id: id.to_string(),
            secret_access_key: "s".to_string(),
            description: String::new(),
            created_at: Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        }
    }

    #[tokio::test]
    async fn snapshot_reflects_credentials_and_tombstones() {
        let s = store().await;
        s.put_credential(&cred("AK")).await.unwrap();
        s.record_control_tombstone("credential", "GONE").await.unwrap();

        let snap = s.build_control_snapshot().await.unwrap();
        assert!(snap.credentials.iter().any(|c| c.credential.access_key_id == "AK"));
        assert!(snap
            .tombstones
            .iter()
            .any(|t| t.entity_type == "credential" && t.entity_key == "GONE"));
        // updated_at is populated (not the epoch).
        let ak = snap
            .credentials
            .iter()
            .find(|c| c.credential.access_key_id == "AK")
            .unwrap();
        assert!(ak.updated_at > DateTime::<Utc>::MIN_UTC);
    }

    #[tokio::test]
    async fn merge_applies_remote_create_and_delete() {
        // Local node B is empty; remote node A has a credential and a deletion.
        let b = store().await;

        // Build a remote snapshot: AK alive, OLD tombstoned.
        let a = store().await;
        a.put_credential(&cred("AK")).await.unwrap();
        a.record_control_tombstone("credential", "OLD").await.unwrap();
        let remote = a.build_control_snapshot().await.unwrap();

        let local = b.build_control_snapshot().await.unwrap();
        let plan = plan_control_merge(&local, &remote);
        b.apply_control_merge(&plan).await.unwrap();

        // B now has AK and adopted the OLD tombstone.
        assert!(b.get_credential("AK").await.unwrap().is_some());
        assert!(b
            .list_control_tombstones()
            .await
            .unwrap()
            .iter()
            .any(|t| t.entity_key == "OLD"));
    }

    #[tokio::test]
    async fn merge_deletes_local_entity_when_peer_tombstone_is_newer() {
        let b = store().await;
        b.put_credential(&cred("AK")).await.unwrap();

        // Remote deleted AK after B created it.
        let a = store().await;
        // Ensure the tombstone is strictly newer than B's credential.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        a.record_control_tombstone("credential", "AK").await.unwrap();
        let remote = a.build_control_snapshot().await.unwrap();

        let local = b.build_control_snapshot().await.unwrap();
        let plan = plan_control_merge(&local, &remote);
        b.apply_control_merge(&plan).await.unwrap();

        // B deleted AK and recorded the tombstone.
        assert!(b.get_credential("AK").await.unwrap().is_none());
        assert!(b
            .list_control_tombstones()
            .await
            .unwrap()
            .iter()
            .any(|t| t.entity_key == "AK"));
    }
}
