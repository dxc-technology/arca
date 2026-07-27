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
    is_node_local_server_config_key, ControlMergePlan, ControlSnapshot, TimestampedBucketConfig,
    TimestampedBucketTags, TimestampedCredential, TimestampedServerConfig, TimestampedTeam,
    TimestampedTeamGrant, TimestampedTeamMember, TimestampedUser, TimestampedUserGrant,
};
use arca_core::error::ArcaError;
use arca_core::store::{
    ControlSnapshotStore, ControlTombstoneStore, CredentialStore, GrantStore, MetadataStore,
    TeamStore, UserStore,
};
use arca_core::types::{Credential, MultipartUploadRecord, PartRecord, Team, User};

use crate::control_merge::apply_control_merge_via_traits;
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::metadata::{row_to_multipart_upload_record, row_to_part_record};
use super::{SqliteStore, TrError};

/// Parses a stored RFC3339 timestamp, mapping the error into rusqlite's space.
fn parse_ts(col: usize, s: &str) -> Result<DateTime<Utc>, rusqlite::Error> {
    DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(col, rusqlite::types::Type::Text, Box::new(e))
        })
}

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

        // R5 (TD-016) + D4 families. Read with their LWW timestamps; the
        // node-local server_config keys never leave this node.
        let (
            user_grants,
            team_grants,
            team_members,
            bucket_configs,
            bucket_tags,
            server_configs,
            multipart_uploads,
            parts,
        ) = self
            .read_conn()
            .call(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT user_id, grant_id, updated_at FROM user_grants")?;
                let user_grants = stmt
                    .query_map([], |row| {
                        Ok(TimestampedUserGrant {
                            user_id: row.get(0)?,
                            grant_id: row.get(1)?,
                            updated_at: parse_ts(2, &row.get::<_, String>(2)?)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                let mut stmt =
                    conn.prepare("SELECT team_id, grant_id, updated_at FROM team_grants")?;
                let team_grants = stmt
                    .query_map([], |row| {
                        Ok(TimestampedTeamGrant {
                            team_id: row.get(0)?,
                            grant_id: row.get(1)?,
                            updated_at: parse_ts(2, &row.get::<_, String>(2)?)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                let mut stmt =
                    conn.prepare("SELECT team_id, user_id, updated_at FROM team_members")?;
                let team_members = stmt
                    .query_map([], |row| {
                        Ok(TimestampedTeamMember {
                            team_id: row.get(0)?,
                            user_id: row.get(1)?,
                            updated_at: parse_ts(2, &row.get::<_, String>(2)?)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                let mut stmt = conn.prepare(
                    "SELECT bucket, config_key, config_value, updated_at FROM bucket_config",
                )?;
                let bucket_configs = stmt
                    .query_map([], |row| {
                        Ok(TimestampedBucketConfig {
                            bucket: row.get(0)?,
                            key: row.get(1)?,
                            value: row.get(2)?,
                            updated_at: parse_ts(3, &row.get::<_, String>(3)?)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;

                // One set-level entity per bucket: rows grouped, timestamp =
                // MAX(updated_at) (equal across the set by construction).
                let mut stmt = conn.prepare(
                    "SELECT bucket, tag_key, tag_value, updated_at FROM bucket_tags ORDER BY bucket, tag_key",
                )?;
                let tag_rows = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            parse_ts(3, &row.get::<_, String>(3)?)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                let mut bucket_tags: Vec<TimestampedBucketTags> = Vec::new();
                for (bucket, k, v, ts) in tag_rows {
                    match bucket_tags.last_mut() {
                        Some(last) if last.bucket == bucket => {
                            last.tags.push((k, v));
                            last.updated_at = last.updated_at.max(ts);
                        }
                        _ => bucket_tags.push(TimestampedBucketTags {
                            bucket,
                            tags: vec![(k, v)],
                            updated_at: ts,
                        }),
                    }
                }

                let mut stmt = conn
                    .prepare("SELECT config_key, config_value, updated_at FROM server_config")?;
                let server_configs = stmt
                    .query_map([], |row| {
                        Ok(TimestampedServerConfig {
                            key: row.get(0)?,
                            value: row.get(1)?,
                            updated_at: parse_ts(2, &row.get::<_, String>(2)?)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .filter(|sc: &TimestampedServerConfig| {
                        !is_node_local_server_config_key(&sc.key)
                    })
                    .collect::<Vec<_>>();

                let mut stmt = conn.prepare(
                    "SELECT upload_id, bucket, key, content_type, initiated_at, metadata, checksum_algorithm FROM multipart_uploads",
                )?;
                let multipart_uploads = stmt
                    .query_map([], |row| row_to_multipart_upload_record(row))?
                    .collect::<Result<Vec<MultipartUploadRecord>, _>>()?;

                let mut stmt = conn.prepare(
                    "SELECT upload_id, part_number, blob_id, size, etag, checksum_value, last_modified FROM parts",
                )?;
                let parts = stmt
                    .query_map([], |row| row_to_part_record(row))?
                    .collect::<Result<Vec<PartRecord>, _>>()?;

                Ok((
                    user_grants,
                    team_grants,
                    team_members,
                    bucket_configs,
                    bucket_tags,
                    server_configs,
                    multipart_uploads,
                    parts,
                ))
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("build_control_snapshot: {e}")))?;

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
        let cred = credential.clone();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO credentials (access_key_id, secret_access_key, description, created_at, active, user_id, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(access_key_id) DO UPDATE SET
                       secret_access_key = excluded.secret_access_key,
                       description = excluded.description,
                       created_at = excluded.created_at,
                       active = excluded.active,
                       user_id = excluded.user_id,
                       updated_at = excluded.updated_at",
                    params![
                        cred.access_key_id,
                        cred.secret_access_key,
                        cred.description,
                        cred.created_at.to_rfc3339(),
                        cred.active as i32,
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

    async fn apply_user_grant_at(
        &self,
        user_id: &str,
        grant_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        let uid = user_id.to_string();
        let gid = grant_id.to_string();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO user_grants (user_id, grant_id, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(user_id, grant_id) DO UPDATE SET updated_at = excluded.updated_at",
                    params![uid, gid, ts],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_user_grant_at: {e}")))
    }

    async fn apply_team_grant_at(
        &self,
        team_id: &str,
        grant_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        let tid = team_id.to_string();
        let gid = grant_id.to_string();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO team_grants (team_id, grant_id, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(team_id, grant_id) DO UPDATE SET updated_at = excluded.updated_at",
                    params![tid, gid, ts],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_team_grant_at: {e}")))
    }

    async fn apply_team_member_at(
        &self,
        team_id: &str,
        user_id: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        let tid = team_id.to_string();
        let uid = user_id.to_string();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO team_members (team_id, user_id, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(team_id, user_id) DO UPDATE SET updated_at = excluded.updated_at",
                    params![tid, uid, ts],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_team_member_at: {e}")))
    }

    async fn apply_server_config_at(
        &self,
        key: &str,
        value: &str,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError> {
        let key = key.to_string();
        let value = value.to_string();
        let ts = updated_at.to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO server_config (config_key, config_value, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(config_key) DO UPDATE SET config_value = ?2, updated_at = ?3",
                    params![key, value, ts],
                )?;
                Ok(())
            })
            .await
            .map_err(|e: TrError| ArcaError::Internal(format!("apply_server_config_at: {e}")))
    }

    async fn apply_control_merge(&self, plan: &ControlMergePlan) -> Result<(), ArcaError> {
        apply_control_merge_via_traits(self, plan).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::cluster::plan_control_merge;
    use arca_core::store::ServerConfigStore;
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

    // --- R5 (TD-016) families -------------------------------------------------

    fn user(id: &str) -> arca_core::types::User {
        arca_core::types::User {
            user_id: id.to_string(),
            username: id.to_string(),
            description: String::new(),
            is_root: false,
            created_at: Utc::now(),
        }
    }

    fn grant(id: &str) -> arca_core::types::Grant {
        arca_core::types::Grant {
            grant_id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            document: arca_core::policy::PolicyDocument {
                version: "2012-10-17".to_string(),
                statement: vec![],
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn team(id: &str) -> arca_core::types::Team {
        arca_core::types::Team {
            team_id: id.to_string(),
            name: id.to_string(),
            description: String::new(),
            created_at: Utc::now(),
        }
    }

    /// Seeds one entity of every R5 family and checks the snapshot carries
    /// them all, with node-local server_config excluded.
    #[tokio::test]
    async fn snapshot_includes_r5_families() {
        use arca_core::types::{BlobId, MultipartUploadRecord, PartRecord};
        let s = store().await;
        s.put_user(&user("u1")).await.unwrap();
        s.put_grant(&grant("g1")).await.unwrap();
        s.put_team(&team("t1")).await.unwrap();
        s.attach_to_user("u1", "g1").await.unwrap();
        s.attach_to_team("t1", "g1").await.unwrap();
        s.add_member("t1", "u1").await.unwrap();
        s.create_bucket("b1").await.unwrap();
        s.set_bucket_config("b1", "versioning", "Enabled").await.unwrap();
        s.put_bucket_tags("b1", &[("env".into(), "dev".into()), ("team".into(), "x".into())])
            .await
            .unwrap();
        s.set_server_config("region", "eu-south-1").await.unwrap();
        s.set_server_config("node_id", "this-node").await.unwrap();
        s.create_multipart_upload(&MultipartUploadRecord {
            upload_id: "up1".into(),
            bucket: "b1".into(),
            key: "k".into(),
            content_type: None,
            initiated_at: Utc::now(),
            metadata: HashMap::new(),
            checksum_algorithm: None,
        })
        .await
        .unwrap();
        s.put_part(&PartRecord {
            upload_id: "up1".into(),
            part_number: 1,
            blob_id: BlobId("blob-1".into()),
            size: 3,
            etag: "e".into(),
            checksum_value: None,
            last_modified: Some(Utc::now()),
        })
        .await
        .unwrap();

        let snap = s.build_control_snapshot().await.unwrap();
        assert!(snap
            .user_grants
            .iter()
            .any(|x| x.user_id == "u1" && x.grant_id == "g1"));
        assert!(snap
            .team_grants
            .iter()
            .any(|x| x.team_id == "t1" && x.grant_id == "g1"));
        assert!(snap
            .team_members
            .iter()
            .any(|x| x.team_id == "t1" && x.user_id == "u1"));
        assert!(snap
            .bucket_configs
            .iter()
            .any(|x| x.bucket == "b1" && x.key == "versioning" && x.value == "Enabled"));
        let tags = snap.bucket_tags.iter().find(|x| x.bucket == "b1").unwrap();
        assert_eq!(tags.tags.len(), 2, "whole set in one entity");
        assert!(snap.server_configs.iter().any(|x| x.key == "region"));
        assert!(
            !snap.server_configs.iter().any(|x| x.key == "node_id"),
            "node-local keys must never leave the node"
        );
        assert!(snap.multipart_uploads.iter().any(|x| x.upload_id == "up1"));
        assert!(snap
            .parts
            .iter()
            .any(|x| x.upload_id == "up1" && x.part_number == 1));
        // Every timestamp is real (not the epoch placeholder).
        assert!(snap.user_grants[0].updated_at > DateTime::<Utc>::MIN_UTC);
    }

    /// The TD-016 scenario: a detach performed while a peer was down must
    /// propagate via the snapshot merge (tombstone) instead of resurrecting.
    #[tokio::test]
    async fn merge_propagates_detach_and_membership_removal() {
        use arca_core::cluster::pair_key;
        use arca_core::store::{TOMBSTONE_TEAM_MEMBER, TOMBSTONE_USER_GRANT};

        // Both nodes start aligned: u1/g1/t1, attach + membership.
        let a = store().await;
        let b = store().await;
        for s in [&a, &b] {
            s.put_user(&user("u1")).await.unwrap();
            s.put_grant(&grant("g1")).await.unwrap();
            s.put_team(&team("t1")).await.unwrap();
            s.attach_to_user("u1", "g1").await.unwrap();
            s.add_member("t1", "u1").await.unwrap();
        }

        // A detaches and removes while B is "down" (the decorator records the
        // tombstones; here we do the same two store calls it performs).
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        a.detach_from_user("u1", "g1").await.unwrap();
        a.record_control_tombstone(TOMBSTONE_USER_GRANT, &pair_key("u1", "g1"))
            .await
            .unwrap();
        a.remove_member("t1", "u1").await.unwrap();
        a.record_control_tombstone(TOMBSTONE_TEAM_MEMBER, &pair_key("t1", "u1"))
            .await
            .unwrap();

        // B re-enters and reconciles from A.
        let remote = a.build_control_snapshot().await.unwrap();
        let local = b.build_control_snapshot().await.unwrap();
        let plan = plan_control_merge(&local, &remote);
        b.apply_control_merge(&plan).await.unwrap();

        assert!(b.list_user_grants("u1").await.unwrap().is_empty(), "detach propagated");
        assert!(b.list_members("t1").await.unwrap().is_empty(), "removal propagated");

        // And the reverse direction must NOT resurrect on A.
        let remote = b.build_control_snapshot().await.unwrap();
        let local = a.build_control_snapshot().await.unwrap();
        let plan = plan_control_merge(&local, &remote);
        a.apply_control_merge(&plan).await.unwrap();
        assert!(a.list_user_grants("u1").await.unwrap().is_empty());
    }

    /// Catch-up: a node that was down when an attachment and a cluster-wide
    /// setting were created pulls them at re-entry, timestamps preserved.
    #[tokio::test]
    async fn merge_catches_up_attachments_and_server_config() {
        let a = store().await;
        a.put_user(&user("u1")).await.unwrap();
        a.put_grant(&grant("g1")).await.unwrap();
        a.attach_to_user("u1", "g1").await.unwrap();
        a.set_server_config("region", "eu-south-1").await.unwrap();

        let b = store().await;
        let remote = a.build_control_snapshot().await.unwrap();
        let local = b.build_control_snapshot().await.unwrap();
        let plan = plan_control_merge(&local, &remote);
        b.apply_control_merge(&plan).await.unwrap();

        assert_eq!(b.list_user_grants("u1").await.unwrap().len(), 1);
        assert_eq!(
            b.get_server_config("region").await.unwrap().as_deref(),
            Some("eu-south-1")
        );
        // LWW timestamps preserved verbatim → the merge reaches a fixed point.
        // Compare the u1:g1 attachment specifically: every store also carries
        // the bootstrap `root → administrator-access` seed row, whose backfill
        // timestamp legitimately differs per store (stamped at migration time).
        let snap_a = a.build_control_snapshot().await.unwrap();
        let snap_b = b.build_control_snapshot().await.unwrap();
        let ug = |s: &ControlSnapshot| {
            s.user_grants
                .iter()
                .find(|x| x.user_id == "u1" && x.grant_id == "g1")
                .expect("u1:g1 attachment present")
                .updated_at
        };
        assert_eq!(ug(&snap_a), ug(&snap_b), "LWW timestamp preserved verbatim");
        // Fixed point in the b←a direction (b's seed row is strictly newer than
        // a's — b was opened later — so ties-keep-local yields no writes).
        assert!(plan_control_merge(&snap_b, &snap_a).is_empty(), "fixed point");
    }

    /// apply_bucket_config_at / apply_bucket_tags_at preserve the source
    /// timestamp verbatim (no now() re-stamp → no flapping).
    #[tokio::test]
    async fn bucket_family_applies_preserve_timestamps() {
        let s = store().await;
        s.create_bucket("b1").await.unwrap();
        let ts = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&Utc);
        s.apply_bucket_config_at("b1", "versioning", "Enabled", ts).await.unwrap();
        s.apply_bucket_tags_at("b1", &[("env".into(), "prod".into())], ts)
            .await
            .unwrap();

        let snap = s.build_control_snapshot().await.unwrap();
        let bc = snap.bucket_configs.iter().find(|x| x.key == "versioning").unwrap();
        assert_eq!(bc.updated_at, ts);
        let bt = snap.bucket_tags.iter().find(|x| x.bucket == "b1").unwrap();
        assert_eq!(bt.updated_at, ts);
        assert_eq!(
            s.get_bucket_config("b1", "versioning").await.unwrap().as_deref(),
            Some("Enabled")
        );
        assert_eq!(s.get_bucket_tags("b1").await.unwrap().len(), 1);
    }
}
