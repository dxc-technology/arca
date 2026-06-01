//! Cluster control-plane store decorators (Phase 29) — identity entities.
//!
//! Wrap the credential and user stores so their mutations replicate to peers
//! via `POST /cluster/v1/op`, keeping the store traits so handlers and
//! `AppState` are unchanged. Same consistency policy as the data plane: in
//! `quorum` mode a mutation is refused with `503` below majority; in
//! `available` mode it always proceeds and fan-out is best-effort.
//!
//! Replicating credentials + users is what makes failover authentication work
//! for runtime-created (non-root) users. (Authorization — grants/teams — and
//! `server_config` reuse this same `/op` channel in a follow-up.)
//!
//! `apply_remote_*` delegate straight to the inner store and never re-fan-out
//! (the `/op` receive path applies through the inner store directly anyway).

use std::sync::Arc;

use arca_core::cluster::{ClusterState, ControlOp};
use arca_core::error::ArcaError;
use arca_core::store::{CredentialStore, UserStore};
use arca_core::types::{Credential, User};
use arca_core::{S3Error, S3ErrorCode};

use crate::cluster::client::ClusterClient;

/// The consistency-policy admission gate, shared by the identity decorators.
/// `available` mode is always `Ok`; `quorum` mode refuses with `503` when too
/// few nodes are live.
fn check_write_quorum(cluster: &ClusterState) -> Result<(), ArcaError> {
    if cluster.has_write_quorum() {
        Ok(())
    } else {
        Err(ArcaError::S3(S3Error::with_message(
            S3ErrorCode::ServiceUnavailable,
            "cluster write quorum not available (too few live nodes)",
            "/",
        )))
    }
}

/// Fan out a control-plane op to every live peer (best-effort; anti-entropy
/// reconciles the rest in M4).
async fn fan_out_op(client: &ClusterClient, cluster: &ClusterState, op: &ControlOp) {
    for peer in cluster.peers().into_iter().filter(|p| p.alive) {
        if let Err(e) = client.send_op(&peer.endpoint, op).await {
            tracing::warn!(
                error = %e,
                peer = %peer.endpoint,
                "cluster control-plane fan-out failed (will reconcile via anti-entropy in M4)"
            );
        }
    }
}

/// Credential store decorator: replicates credential mutations to peers.
pub struct ClusterCredentialStore {
    inner: Arc<dyn CredentialStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
}

impl ClusterCredentialStore {
    pub fn new(
        inner: Arc<dyn CredentialStore>,
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
impl CredentialStore for ClusterCredentialStore {
    async fn put_credential(&self, credential: &Credential) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.put_credential(credential).await?;
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
}

impl ClusterUserStore {
    pub fn new(
        inner: Arc<dyn UserStore>,
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
impl UserStore for ClusterUserStore {
    async fn put_user(&self, user: &User) -> Result<(), ArcaError> {
        check_write_quorum(&self.cluster)?;
        self.inner.put_user(user).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn temp_credentials() -> (Arc<dyn CredentialStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.db");
        let store = arca_storage::SqliteStore::open(&db).await.unwrap();
        (Arc::new(store), dir)
    }

    fn client() -> ClusterClient {
        ClusterClient::new("self-node", "secret", Duration::from_secs(1)).unwrap()
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
        let cluster = Arc::new(ClusterState::new("self-node", Some(2)));
        let store = ClusterCredentialStore::new(inner, client(), cluster);
        let err = store.put_credential(&sample_credential()).await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn credential_available_mode_writes_alone() {
        let (inner, _dir) = temp_credentials().await;
        let cluster = Arc::new(ClusterState::new("self-node", None));
        let store = ClusterCredentialStore::new(inner, client(), cluster);
        // available mode: write succeeds solo; no peers -> no fan-out.
        store.put_credential(&sample_credential()).await.unwrap();
        assert!(store.get_credential("K").await.unwrap().is_some());
    }
}
