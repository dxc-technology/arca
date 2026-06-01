//! Cluster metadata store decorator (Phase 29 M3 — data-plane write path).
//!
//! Wraps the local `MetadataStore` and replicates object-table mutations to
//! peers, keeping the trait so handlers and `AppState` are unchanged. It is the
//! linearization point for the consistency policy: every replicated write
//! passes the mode gate first.
//!
//! Replicated (object data plane):
//! - `put_object` — create/overwrite, including replicated rows for any
//!   versioning state (origin mints the canonical `version_id`).
//! - `delete_object` — hard delete (unversioned) → version delete; or a
//!   versioned delete marker → replicated as a row.
//! - `delete_object_version` — hard delete of a specific version.
//!
//! Mode gate ([`ClusterState::has_write_quorum`]):
//! - `available`: always writable (gate is a no-op).
//! - `quorum`: when live nodes < majority, replicated writes are refused with
//!   `503 ServiceUnavailable` (the node stays read-only) — no divergence.
//!
//! NOT yet cluster-aware (delegate only — tracked for the follow-up chunks):
//! object tags, retention/legal-hold, multipart, and all bucket /
//! `bucket_config` mutations (the latter are control-plane, replicated via
//! `/cluster/v1/op` once that lands). Until bucket creation replicates,
//! replicated object rows are not yet servable on a peer that lacks the bucket;
//! the end-to-end path completes with the control-plane chunk.
//!
//! `apply_remote_*` delegate straight to the inner store and never re-fan-out
//! (they apply rows already received from a peer).

use std::sync::Arc;

use arca_core::cluster::ClusterState;
use arca_core::error::ArcaError;
use arca_core::store::MetadataStore;
use arca_core::types::{
    BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats,
};
use arca_core::{S3Error, S3ErrorCode};
use chrono::{DateTime, Utc};

use crate::cluster::client::ClusterClient;

/// Metadata store decorator that replicates object-table mutations to peers
/// under the configured consistency policy.
pub struct ClusterMetadataStore {
    inner: Arc<dyn MetadataStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
}

impl ClusterMetadataStore {
    pub fn new(
        inner: Arc<dyn MetadataStore>,
        client: ClusterClient,
        cluster: Arc<ClusterState>,
    ) -> Self {
        Self {
            inner,
            client,
            cluster,
        }
    }

    /// Endpoints of peers currently considered alive.
    fn live_peers(&self) -> Vec<String> {
        self.cluster
            .peers()
            .into_iter()
            .filter(|p| p.alive)
            .map(|p| p.endpoint)
            .collect()
    }

    /// The consistency-policy admission gate. In `available` mode this is always
    /// `Ok`; in `quorum` mode it refuses with `503` when too few nodes are live.
    fn check_write_quorum(&self) -> Result<(), ArcaError> {
        if self.cluster.has_write_quorum() {
            Ok(())
        } else {
            Err(ArcaError::S3(S3Error::with_message(
                S3ErrorCode::ServiceUnavailable,
                "cluster write quorum not available (too few live nodes)",
                "/",
            )))
        }
    }

    /// Replicates a fully-formed object row to every live peer (best-effort;
    /// anti-entropy reconciles the rest in M4).
    async fn fan_out_object(&self, record: &ObjectRecord) {
        for endpoint in self.live_peers() {
            if let Err(e) = self.client.send_object(&endpoint, record).await {
                tracing::warn!(
                    error = %e,
                    peer = %endpoint,
                    bucket = %record.bucket,
                    key = %record.key,
                    "cluster object fan-out failed (will reconcile via anti-entropy in M4)"
                );
            }
        }
    }

    /// Replicates a version hard-delete to every live peer (best-effort).
    async fn fan_out_version_delete(&self, bucket: &str, key: &str, version_id: &str) {
        for endpoint in self.live_peers() {
            if let Err(e) = self
                .client
                .send_version_delete(&endpoint, bucket, key, version_id)
                .await
            {
                tracing::warn!(
                    error = %e,
                    peer = %endpoint,
                    bucket = %bucket,
                    key = %key,
                    "cluster delete fan-out failed (will reconcile via anti-entropy in M4)"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl MetadataStore for ClusterMetadataStore {
    // -- Bucket operations (delegate; control-plane replication is a follow-up) --

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, ArcaError> {
        self.inner.list_buckets().await
    }

    async fn create_bucket(&self, name: &str) -> Result<(), ArcaError> {
        self.inner.create_bucket(name).await
    }

    async fn head_bucket(&self, name: &str) -> Result<Option<BucketInfo>, ArcaError> {
        self.inner.head_bucket(name).await
    }

    async fn delete_bucket(&self, name: &str) -> Result<bool, ArcaError> {
        self.inner.delete_bucket(name).await
    }

    async fn bucket_is_empty(&self, name: &str) -> Result<bool, ArcaError> {
        self.inner.bucket_is_empty(name).await
    }

    async fn get_stats(&self) -> Result<StorageStats, ArcaError> {
        self.inner.get_stats().await
    }

    // -- Object operations (replicated) --

    async fn put_object(
        &self,
        record: &ObjectRecord,
    ) -> Result<(Option<ObjectRecord>, Option<String>), ArcaError> {
        self.check_write_quorum()?;
        let (old, version_id) = self.inner.put_object(record).await?;

        // Replicate the row exactly as stored: the input fields plus the
        // canonical version_id the inner store assigned. is_delete_marker is
        // forced false to mirror put_object; is_latest=true (the peer's
        // apply_remote_object recompute finalizes it deterministically).
        let mut replicated = record.clone();
        replicated.version_id = version_id.clone();
        replicated.is_latest = true;
        replicated.is_delete_marker = false;
        self.fan_out_object(&replicated).await;

        Ok((old, version_id))
    }

    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        self.inner.get_object(bucket, key).await
    }

    async fn get_latest_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        self.inner.get_latest_object(bucket, key).await
    }

    async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        self.check_write_quorum()?;
        let old = self.inner.delete_object(bucket, key).await?;
        if old.is_some() {
            // A versioned bucket creates a delete marker (a new latest row);
            // an unversioned bucket hard-deletes. Replicate whichever happened.
            match self.inner.get_latest_object(bucket, key).await {
                Ok(Some(marker)) if marker.is_delete_marker => {
                    self.fan_out_object(&marker).await;
                }
                _ => {
                    let version_id = old
                        .as_ref()
                        .and_then(|o| o.version_id.clone())
                        .unwrap_or_else(|| "null".to_string());
                    self.fan_out_version_delete(bucket, key, &version_id).await;
                }
            }
        }
        Ok(old)
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_objects(bucket, prefix, start_after, max_keys)
            .await
    }

    // -- Versioned object operations --

    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        self.inner.get_object_version(bucket, key, version_id).await
    }

    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        self.check_write_quorum()?;
        let deleted = self
            .inner
            .delete_object_version(bucket, key, version_id)
            .await?;
        if deleted.is_some() {
            self.fan_out_version_delete(bucket, key, version_id).await;
        }
        Ok(deleted)
    }

    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        version_id_marker: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_object_versions(bucket, prefix, key_marker, version_id_marker, max_keys)
            .await
    }

    // -- Multipart upload operations (delegate; replication is a follow-up) --

    async fn create_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        self.inner.create_multipart_upload(record).await
    }

    async fn get_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Option<MultipartUploadRecord>, ArcaError> {
        self.inner.get_multipart_upload(upload_id).await
    }

    async fn put_part(&self, part: &PartRecord) -> Result<Option<PartRecord>, ArcaError> {
        self.inner.put_part(part).await
    }

    async fn list_parts(&self, upload_id: &str) -> Result<Vec<PartRecord>, ArcaError> {
        self.inner.list_parts(upload_id).await
    }

    async fn delete_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, ArcaError> {
        self.inner.delete_multipart_upload(upload_id).await
    }

    // -- Object Lock operations (delegate; replication is a follow-up) --

    async fn set_object_retention(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        retention_mode: Option<&str>,
        retain_until_date: Option<&str>,
    ) -> Result<bool, ArcaError> {
        self.inner
            .set_object_retention(bucket, key, version_id, retention_mode, retain_until_date)
            .await
    }

    async fn set_object_legal_hold(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool, ArcaError> {
        self.inner
            .set_object_legal_hold(bucket, key, version_id, status)
            .await
    }

    // -- Bucket config operations (delegate; control-plane follow-up) --

    async fn get_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<Option<String>, ArcaError> {
        self.inner.get_bucket_config(bucket, config_key).await
    }

    async fn set_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
    ) -> Result<(), ArcaError> {
        self.inner
            .set_bucket_config(bucket, config_key, config_value)
            .await
    }

    async fn delete_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<bool, ArcaError> {
        self.inner.delete_bucket_config(bucket, config_key).await
    }

    // -- Tag operations (delegate; replication is a follow-up) --

    async fn get_bucket_tags(&self, bucket: &str) -> Result<Vec<(String, String)>, ArcaError> {
        self.inner.get_bucket_tags(bucket).await
    }

    async fn put_bucket_tags(
        &self,
        bucket: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        self.inner.put_bucket_tags(bucket, tags).await
    }

    async fn delete_bucket_tags(&self, bucket: &str) -> Result<bool, ArcaError> {
        self.inner.delete_bucket_tags(bucket).await
    }

    async fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Vec<(String, String)>, ArcaError> {
        self.inner.get_object_tags(bucket, key, version_id).await
    }

    async fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        self.inner
            .put_object_tags(bucket, key, version_id, tags)
            .await
    }

    async fn delete_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<bool, ArcaError> {
        self.inner.delete_object_tags(bucket, key, version_id).await
    }

    // -- Lifecycle query operations (read-only; delegate) --

    async fn list_expired_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        tags: &[(String, String)],
        cutoff: DateTime<Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_expired_objects(bucket, prefix, tags, cutoff, start_after, max_keys)
            .await
    }

    async fn list_noncurrent_expired_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        cutoff: DateTime<Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_noncurrent_expired_versions(bucket, prefix, cutoff, start_after, max_keys)
            .await
    }

    async fn list_stale_multipart_uploads(
        &self,
        bucket: &str,
        cutoff: DateTime<Utc>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        self.inner
            .list_stale_multipart_uploads(bucket, cutoff, max_uploads)
            .await
    }

    async fn list_multipart_uploads(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        upload_id_marker: Option<&str>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        self.inner
            .list_multipart_uploads(bucket, prefix, key_marker, upload_id_marker, max_uploads)
            .await
    }

    // -- Cluster replication: apply received rows locally, never re-fan-out --

    async fn apply_remote_object(&self, record: &ObjectRecord) -> Result<(), ArcaError> {
        self.inner.apply_remote_object(record).await
    }

    async fn apply_remote_version_delete(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), ArcaError> {
        self.inner
            .apply_remote_version_delete(bucket, key, version_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::cluster::{ClusterState, PeerNode};
    use arca_core::types::BlobId;
    use std::time::Duration;

    fn sample_record() -> ObjectRecord {
        ObjectRecord {
            bucket: "b".to_string(),
            key: "k".to_string(),
            blob_id: BlobId("blob-1".to_string()),
            size: 4,
            etag: "e".to_string(),
            content_type: None,
            last_modified: Utc::now(),
            metadata: Default::default(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            replication_status: None,
        }
    }

    /// A real SQLite store as the inner backend (no mock), kept alive by the
    /// returned TempDir.
    async fn temp_store() -> (Arc<dyn MetadataStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meta.db");
        let store = arca_storage::SqliteStore::open(&db).await.unwrap();
        (Arc::new(store), dir)
    }

    fn client() -> ClusterClient {
        ClusterClient::new("self-node", "secret", Duration::from_secs(1)).unwrap()
    }

    fn live_peer() -> PeerNode {
        PeerNode {
            node_id: "peer-2".to_string(),
            // Unreachable on purpose: fan-out is best-effort and must not fail
            // the local write (connection refused returns immediately).
            endpoint: "http://127.0.0.1:1".to_string(),
            alive: true,
            last_seen: None,
        }
    }

    #[tokio::test]
    async fn quorum_mode_refuses_write_without_majority() {
        let (inner, _dir) = temp_store().await;
        // cluster_size=3 -> write_quorum=2; alone (no live peers) -> 1 < 2 -> read-only.
        let cluster = Arc::new(ClusterState::new("self-node", Some(2)));
        let store = ClusterMetadataStore::new(inner, client(), cluster);
        let err = store.put_object(&sample_record()).await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn available_mode_writes_alone() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        // available mode (write_quorum=None) -> always writable, even solo.
        let cluster = Arc::new(ClusterState::new("self-node", None));
        let store = ClusterMetadataStore::new(inner, client(), cluster);
        store.put_object(&sample_record()).await.unwrap();
        assert!(store.get_object("b", "k").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn quorum_mode_writes_with_majority_despite_unreachable_peer() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let cluster = Arc::new(ClusterState::new("self-node", Some(2)));
        cluster.set_peers(vec![live_peer()]); // self + 1 = 2 >= 2 -> quorum met
        let store = ClusterMetadataStore::new(inner, client(), cluster);
        // Gate passes; the fan-out to the unreachable peer fails silently
        // (best-effort, reconciled by anti-entropy in M4); the local write stands.
        store.put_object(&sample_record()).await.unwrap();
        assert!(store.get_object("b", "k").await.unwrap().is_some());
    }
}
