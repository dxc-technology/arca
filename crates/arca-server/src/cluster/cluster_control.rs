//! Cluster control-plane store decorators (Phase 29) — identity entities.
//!
//! Wrap the credential and user stores so their mutations replicate to peers
//! via `POST /cluster/v1/op`, keeping the store traits so handlers and
//! `AppState` are unchanged. Same consistency policy as the data plane: in
//! `quorum` mode a mutation is refused with `503` below majority; in
//! `available` mode it always proceeds and fan-out is best-effort.
//!
//! Replicating credentials + users is what makes failover authentication work
//! for runtime-created (non-root) users. Authorization (grants/teams) and the
//! cluster-wide `server_config` settings reuse this same `/op` channel.
//!
//! `apply_remote_*` delegate straight to the inner store and never re-fan-out
//! (the `/op` receive path applies through the inner store directly anyway).

use std::sync::Arc;

use arca_core::cluster::is_node_local_server_config_key as is_node_local_key;
use arca_core::cluster::{ClusterState, ControlOp};
use arca_core::error::ArcaError;
use arca_core::policy::PolicyDocument;
use arca_core::store::{
    ControlTombstoneStore, CredentialStore, GrantStore, ServerConfigStore, TeamStore, UserStore,
    TOMBSTONE_CREDENTIAL, TOMBSTONE_GRANT, TOMBSTONE_TEAM, TOMBSTONE_USER,
};
use arca_core::types::{Credential, Grant, Team, User};

use crate::cluster::client::ClusterClient;
use crate::cluster::cluster_meta::check_write_gate;

/// The consistency-policy admission gate, shared by the identity decorators.
/// `available` mode is always `Ok`; `quorum` mode refuses with `503` when too
/// few ELIGIBLE nodes are live, or when more eligible nodes than
/// `cluster_size` are live (H6 fail-closed) — see `check_write_gate`.
fn check_write_quorum(cluster: &ClusterState) -> Result<(), ArcaError> {
    check_write_gate(cluster)
}

/// Fan out a control-plane op to every ELIGIBLE peer IN PARALLEL (§2.4):
/// alive, authenticated (decision H12 — never hand identity state to a peer
/// that has not proven possession of the cluster secret) and config-aligned.
/// Best-effort by design (decision H4): identity mutations are rare and the
/// anti-entropy control-snapshot reconcile heals what a peer missed.
async fn fan_out_op(client: &ClusterClient, cluster: &ClusterState, op: &ControlOp) {
    let sends = cluster
        .peers()
        .into_iter()
        .filter(|p| p.eligible())
        .map(|peer| async move {
            if let Err(e) = client.send_op(&peer.endpoint, op).await {
                tracing::warn!(
                    error = %e,
                    peer = %peer.endpoint,
                    "cluster control-plane fan-out failed (will reconcile via anti-entropy)"
                );
            }
        });
    futures_util::future::join_all(sends).await;
}

/// Records a deletion tombstone for a control-plane entity so the delete
/// converges via the control-snapshot reconcile and is not resurrected by a
/// peer that still holds the live row. Local write, best-effort: the entity is
/// already deleted; a failure here is logged, not propagated.
async fn record_tombstone(
    tombstones: &Arc<dyn ControlTombstoneStore>,
    entity_type: &str,
    entity_key: &str,
) {
    if let Err(e) = tombstones
        .record_control_tombstone(entity_type, entity_key)
        .await
    {
        tracing::warn!(
            error = %e,
            entity_type,
            entity_key,
            "failed to record control-plane tombstone (delete may be resurrected by reconcile)"
        );
    }
}

/// Clears any stale tombstone for an entity that is being (re-)created locally,
/// so the reconcile does not later re-delete the fresh entity. Best-effort.
async fn clear_tombstone(
    tombstones: &Arc<dyn ControlTombstoneStore>,
    entity_type: &str,
    entity_key: &str,
) {
    if let Err(e) = tombstones
        .delete_control_tombstone(entity_type, entity_key)
        .await
    {
        tracing::warn!(
            error = %e,
            entity_type,
            entity_key,
            "failed to clear stale control-plane tombstone on re-create"
        );
    }
}

/// Credential store decorator: replicates credential mutations to peers.
pub struct ClusterCredentialStore {
    inner: Arc<dyn CredentialStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
    tombstones: Arc<dyn ControlTombstoneStore>,
}

impl ClusterCredentialStore {
    pub fn new(
        inner: Arc<dyn CredentialStore>,
        client: ClusterClient,
        cluster: Arc<ClusterState>,
        tombstones: Arc<dyn ControlTombstoneStore>,
    ) -> Self {
        Self {
            inner,
            client,
            cluster,
            tombstones,
        }
    }
}

#[async_trait::async_trait]
impl CredentialStore for ClusterCredentialStore {
    async fn put_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.put_credential(credential).await?;
        clear_tombstone(&self.tombstones, TOMBSTONE_CREDENTIAL, &credential.access_key_id).await;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::CredentialUpsert {
                credential: credential.clone(),
            },
        )
        .await;
        Ok(())
    }

    async fn get_credential(
        &self,
        access_key_id: &str,
    ) -> Result<Option<Credential>, ArcaError> {
        self.inner.get_credential(access_key_id).await
    }

    async fn list_credentials(&self) -> Result<Vec<Credential>, ArcaError> {
        self.inner.list_credentials().await
    }

    async fn delete_credential(&self, access_key_id: &str) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.delete_credential(access_key_id).await?;
        if existed {
            record_tombstone(&self.tombstones, TOMBSTONE_CREDENTIAL, access_key_id).await;
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::CredentialDelete {
                    access_key_id: access_key_id.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn update_credential(
        &self,
        access_key_id: &str,
        active: Option<bool>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let ok = self
            .inner
            .update_credential(access_key_id, active, description)
            .await?;
        if ok {
            // Re-read the full row and replicate it verbatim (update only touches
            // some fields; peers get the complete post-update credential).
            if let Ok(Some(credential)) = self.inner.get_credential(access_key_id).await {
                fan_out_op(
                    &self.client,
                    &self.cluster,
                    &ControlOp::CredentialUpsert { credential },
                )
                .await;
            }
        }
        Ok(ok)
    }

    async fn count_active_credentials(&self) -> Result<u64, ArcaError> {
        self.inner.count_active_credentials().await
    }

    async fn apply_remote_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        self.inner.apply_remote_credential(credential).await
    }
}

/// User store decorator: replicates user mutations to peers.
pub struct ClusterUserStore {
    inner: Arc<dyn UserStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
    tombstones: Arc<dyn ControlTombstoneStore>,
}

impl ClusterUserStore {
    pub fn new(
        inner: Arc<dyn UserStore>,
        client: ClusterClient,
        cluster: Arc<ClusterState>,
        tombstones: Arc<dyn ControlTombstoneStore>,
    ) -> Self {
        Self {
            inner,
            client,
            cluster,
            tombstones,
        }
    }
}

#[async_trait::async_trait]
impl UserStore for ClusterUserStore {
    async fn put_user(&self, user: &User) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.put_user(user).await?;
        clear_tombstone(&self.tombstones, TOMBSTONE_USER, &user.user_id).await;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::UserUpsert { user: user.clone() },
        )
        .await;
        Ok(())
    }

    async fn get_user(&self, user_id: &str) -> Result<Option<User>, ArcaError> {
        self.inner.get_user(user_id).await
    }

    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, ArcaError> {
        self.inner.get_user_by_username(username).await
    }

    async fn list_users(&self) -> Result<Vec<User>, ArcaError> {
        self.inner.list_users().await
    }

    async fn update_user(
        &self,
        user_id: &str,
        username: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let ok = self.inner.update_user(user_id, username, description).await?;
        if ok {
            if let Ok(Some(user)) = self.inner.get_user(user_id).await {
                fan_out_op(
                    &self.client,
                    &self.cluster,
                    &ControlOp::UserUpsert { user },
                )
                .await;
            }
        }
        Ok(ok)
    }

    async fn delete_user(&self, user_id: &str) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.delete_user(user_id).await?;
        if existed {
            record_tombstone(&self.tombstones, TOMBSTONE_USER, user_id).await;
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::UserDelete {
                    user_id: user_id.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn apply_remote_user(&self, user: &User) -> Result<(), ArcaError> {
        self.inner.apply_remote_user(user).await
    }
}

/// Grant store decorator: replicates grant (policy) and attachment mutations.
pub struct ClusterGrantStore {
    inner: Arc<dyn GrantStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
    tombstones: Arc<dyn ControlTombstoneStore>,
}

impl ClusterGrantStore {
    pub fn new(
        inner: Arc<dyn GrantStore>,
        client: ClusterClient,
        cluster: Arc<ClusterState>,
        tombstones: Arc<dyn ControlTombstoneStore>,
    ) -> Self {
        Self {
            inner,
            client,
            cluster,
            tombstones,
        }
    }
}

#[async_trait::async_trait]
impl GrantStore for ClusterGrantStore {
    async fn put_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.put_grant(grant).await?;
        clear_tombstone(&self.tombstones, TOMBSTONE_GRANT, &grant.grant_id).await;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::GrantUpsert {
                grant: grant.clone(),
            },
        )
        .await;
        Ok(())
    }

    async fn get_grant(&self, grant_id: &str) -> Result<Option<Grant>, ArcaError> {
        self.inner.get_grant(grant_id).await
    }

    async fn get_grant_by_name(&self, name: &str) -> Result<Option<Grant>, ArcaError> {
        self.inner.get_grant_by_name(name).await
    }

    async fn list_grants(&self) -> Result<Vec<Grant>, ArcaError> {
        self.inner.list_grants().await
    }

    async fn update_grant(
        &self,
        grant_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        document: Option<&PolicyDocument>,
    ) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let ok = self
            .inner
            .update_grant(grant_id, name, description, document)
            .await?;
        if ok {
            if let Ok(Some(grant)) = self.inner.get_grant(grant_id).await {
                fan_out_op(&self.client, &self.cluster, &ControlOp::GrantUpsert { grant }).await;
            }
        }
        Ok(ok)
    }

    async fn delete_grant(&self, grant_id: &str) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.delete_grant(grant_id).await?;
        if existed {
            record_tombstone(&self.tombstones, TOMBSTONE_GRANT, grant_id).await;
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::GrantDelete {
                    grant_id: grant_id.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn attach_to_user(&self, user_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.attach_to_user(user_id, grant_id).await?;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::UserGrantAttach {
                user_id: user_id.to_string(),
                grant_id: grant_id.to_string(),
            },
        )
        .await;
        Ok(())
    }

    async fn detach_from_user(&self, user_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.detach_from_user(user_id, grant_id).await?;
        if existed {
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::UserGrantDetach {
                    user_id: user_id.to_string(),
                    grant_id: grant_id.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn attach_to_team(&self, team_id: &str, grant_id: &str) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.attach_to_team(team_id, grant_id).await?;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::TeamGrantAttach {
                team_id: team_id.to_string(),
                grant_id: grant_id.to_string(),
            },
        )
        .await;
        Ok(())
    }

    async fn detach_from_team(&self, team_id: &str, grant_id: &str) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.detach_from_team(team_id, grant_id).await?;
        if existed {
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::TeamGrantDetach {
                    team_id: team_id.to_string(),
                    grant_id: grant_id.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn list_user_grants(&self, user_id: &str) -> Result<Vec<Grant>, ArcaError> {
        self.inner.list_user_grants(user_id).await
    }

    async fn list_team_grants(&self, team_id: &str) -> Result<Vec<Grant>, ArcaError> {
        self.inner.list_team_grants(team_id).await
    }

    async fn get_effective_policies(
        &self,
        user_id: &str,
    ) -> Result<Vec<PolicyDocument>, ArcaError> {
        self.inner.get_effective_policies(user_id).await
    }

    async fn apply_remote_grant(&self, grant: &Grant) -> Result<(), ArcaError> {
        self.inner.apply_remote_grant(grant).await
    }
}

/// Team store decorator: replicates team and membership mutations.
pub struct ClusterTeamStore {
    inner: Arc<dyn TeamStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
    tombstones: Arc<dyn ControlTombstoneStore>,
}

impl ClusterTeamStore {
    pub fn new(
        inner: Arc<dyn TeamStore>,
        client: ClusterClient,
        cluster: Arc<ClusterState>,
        tombstones: Arc<dyn ControlTombstoneStore>,
    ) -> Self {
        Self {
            inner,
            client,
            cluster,
            tombstones,
        }
    }
}

#[async_trait::async_trait]
impl TeamStore for ClusterTeamStore {
    async fn put_team(&self, team: &Team) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.put_team(team).await?;
        clear_tombstone(&self.tombstones, TOMBSTONE_TEAM, &team.team_id).await;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::TeamUpsert { team: team.clone() },
        )
        .await;
        Ok(())
    }

    async fn get_team(&self, team_id: &str) -> Result<Option<Team>, ArcaError> {
        self.inner.get_team(team_id).await
    }

    async fn list_teams(&self) -> Result<Vec<Team>, ArcaError> {
        self.inner.list_teams().await
    }

    async fn update_team(
        &self,
        team_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let ok = self.inner.update_team(team_id, name, description).await?;
        if ok {
            if let Ok(Some(team)) = self.inner.get_team(team_id).await {
                fan_out_op(&self.client, &self.cluster, &ControlOp::TeamUpsert { team }).await;
            }
        }
        Ok(ok)
    }

    async fn delete_team(&self, team_id: &str) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.delete_team(team_id).await?;
        if existed {
            record_tombstone(&self.tombstones, TOMBSTONE_TEAM, team_id).await;
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::TeamDelete {
                    team_id: team_id.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn add_member(&self, team_id: &str, user_id: &str) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.add_member(team_id, user_id).await?;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::TeamMemberAdd {
                team_id: team_id.to_string(),
                user_id: user_id.to_string(),
            },
        )
        .await;
        Ok(())
    }

    async fn remove_member(&self, team_id: &str, user_id: &str) -> Result<bool, ArcaError> {
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.remove_member(team_id, user_id).await?;
        if existed {
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::TeamMemberRemove {
                    team_id: team_id.to_string(),
                    user_id: user_id.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn list_members(&self, team_id: &str) -> Result<Vec<User>, ArcaError> {
        self.inner.list_members(team_id).await
    }

    async fn list_user_teams(&self, user_id: &str) -> Result<Vec<Team>, ArcaError> {
        self.inner.list_user_teams(user_id).await
    }

    async fn apply_remote_team(&self, team: &Team) -> Result<(), ArcaError> {
        self.inner.apply_remote_team(team).await
    }
}

/// Server-config store decorator: replicates cluster-wide instance settings to
/// peers. Node-local keys (see
/// [`arca_core::cluster::is_node_local_server_config_key`], shared with the
/// D12.1 receive-side filter) are persisted locally only — they skip both the
/// quorum gate and the fan-out, so node identity bootstrap works even when the
/// cluster has no write quorum.
pub struct ClusterServerConfigStore {
    inner: Arc<dyn ServerConfigStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
}

impl ClusterServerConfigStore {
    pub fn new(
        inner: Arc<dyn ServerConfigStore>,
        client: ClusterClient,
        cluster: Arc<ClusterState>,
    ) -> Self {
        Self {
            inner,
            client,
            cluster,
        }
    }
}

#[async_trait::async_trait]
impl ServerConfigStore for ClusterServerConfigStore {
    async fn get_server_config(&self, key: &str) -> Result<Option<String>, ArcaError> {
        self.inner.get_server_config(key).await
    }

    async fn set_server_config(&self, key: &str, value: &str) -> Result<(), ArcaError> {
        // Node-local keys never leave this node and never gate on quorum.
        if is_node_local_key(key) {
            return self.inner.set_server_config(key, value).await;
        }
        check_write_quorum(&self.cluster)?;
        self.inner.set_server_config(key, value).await?;
        fan_out_op(
            &self.client,
            &self.cluster,
            &ControlOp::ServerConfigSet {
                key: key.to_string(),
                value: value.to_string(),
            },
        )
        .await;
        Ok(())
    }

    async fn delete_server_config(&self, key: &str) -> Result<bool, ArcaError> {
        if is_node_local_key(key) {
            return self.inner.delete_server_config(key).await;
        }
        check_write_quorum(&self.cluster)?;
        let existed = self.inner.delete_server_config(key).await?;
        if existed {
            fan_out_op(
                &self.client,
                &self.cluster,
                &ControlOp::ServerConfigDelete {
                    key: key.to_string(),
                },
            )
            .await;
        }
        Ok(existed)
    }

    async fn list_server_config(&self) -> Result<Vec<(String, String)>, ArcaError> {
        self.inner.list_server_config().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::cluster::NODE_ID_KEY;
    use arca_core::S3ErrorCode;
    use std::time::Duration;

    async fn temp_credentials() -> (Arc<dyn CredentialStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.db");
        let store = arca_storage::SqliteStore::open(&db).await.unwrap();
        (Arc::new(store), dir)
    }

    /// A standalone in-memory tombstone store for decorator construction in
    /// tests (the gate tests do not exercise tombstone recording).
    async fn tombstones() -> Arc<dyn ControlTombstoneStore> {
        Arc::new(arca_storage::SqliteStore::open_in_memory().await.unwrap())
    }

    fn client() -> ClusterClient {
        ClusterClient::new("self-node", "secret", Duration::from_secs(1), None).unwrap()
    }

    fn sample_credential() -> Credential {
        Credential {
            access_key_id: "K".to_string(),
            secret_access_key: "s".to_string(),
            description: String::new(),
            created_at: chrono::Utc::now(),
            active: true,
            admin: false,
            user_id: "root".to_string(),
        }
    }

    #[tokio::test]
    async fn credential_quorum_gate_refuses_without_majority() {
        let (inner, _dir) = temp_credentials().await;
        // cluster_size=3 -> quorum=2; alone -> identity writes are refused too.
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        let store = ClusterCredentialStore::new(inner, client(), cluster, tombstones().await);
        let err = store.put_credential(&sample_credential()).await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn credential_available_mode_writes_alone() {
        let (inner, _dir) = temp_credentials().await;
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        let store = ClusterCredentialStore::new(inner, client(), cluster, tombstones().await);
        // available mode: write succeeds solo; no peers -> no fan-out.
        store.put_credential(&sample_credential()).await.unwrap();
        assert!(store.get_credential("K").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn delete_records_tombstone_and_create_clears_it() {
        // Same underlying store serves as both credential and tombstone store.
        let store = Arc::new(arca_storage::SqliteStore::open_in_memory().await.unwrap());
        let creds: Arc<dyn CredentialStore> = store.clone();
        let tomb: Arc<dyn ControlTombstoneStore> = store.clone();
        let cluster = Arc::new(ClusterState::new("self-node", None, None)); // available mode
        let dec = ClusterCredentialStore::new(creds, client(), cluster, tomb.clone());

        dec.put_credential(&sample_credential()).await.unwrap();
        assert!(tomb.list_control_tombstones().await.unwrap().is_empty());

        // Delete records a tombstone.
        assert!(dec.delete_credential("K").await.unwrap());
        let list = tomb.list_control_tombstones().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].entity_type, TOMBSTONE_CREDENTIAL);
        assert_eq!(list[0].entity_key, "K");

        // Re-creating the same entity clears the stale tombstone.
        dec.put_credential(&sample_credential()).await.unwrap();
        assert!(tomb.list_control_tombstones().await.unwrap().is_empty());
    }

    async fn temp_server_config() -> (Arc<dyn ServerConfigStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sc.db");
        let store = arca_storage::SqliteStore::open(&db).await.unwrap();
        (Arc::new(store), dir)
    }

    #[test]
    fn only_node_id_is_node_local() {
        assert!(is_node_local_key(NODE_ID_KEY));
        assert!(!is_node_local_key("region"));
        assert!(!is_node_local_key("log_level"));
        assert!(!is_node_local_key("audit_retention_days"));
    }

    #[tokio::test]
    async fn server_config_node_local_key_bypasses_quorum_gate() {
        let (inner, _dir) = temp_server_config().await;
        // No write quorum (alone in a 3-node cluster): cluster-wide settings are
        // refused, but the node-local node_id must still persist for bootstrap.
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        let store = ClusterServerConfigStore::new(inner, client(), cluster);
        store
            .set_server_config(NODE_ID_KEY, "abc-123")
            .await
            .expect("node-local key must bypass the quorum gate");
        assert_eq!(
            store.get_server_config(NODE_ID_KEY).await.unwrap().as_deref(),
            Some("abc-123")
        );
        // A cluster-wide key is gated when quorum is unavailable.
        let err = store.set_server_config("region", "eu").await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn server_config_available_mode_writes_alone() {
        let (inner, _dir) = temp_server_config().await;
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        let store = ClusterServerConfigStore::new(inner, client(), cluster);
        // available mode: cluster-wide write succeeds solo; no peers -> no fan-out.
        store.set_server_config("region", "eu-west-1").await.unwrap();
        assert_eq!(
            store.get_server_config("region").await.unwrap().as_deref(),
            Some("eu-west-1")
        );
    }
}
