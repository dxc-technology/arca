//! Cluster metadata store decorator (Phase 29 M3 — data-plane write path).
//!
//! Wraps the local `MetadataStore` and replicates object-table mutations to
//! peers, keeping the trait so handlers and `AppState` are unchanged. It is the
//! linearization point for the consistency policy.
//!
//! Replicated (object data plane):
//! - `put_object` — create/overwrite, including replicated rows for any
//!   versioning state (origin mints the canonical `version_id`).
//! - `delete_object` — hard delete (unversioned) → version delete; or a
//!   versioned delete marker → replicated as a row.
//! - `delete_object_version` — hard delete of a specific version.
//!
//! Consistency policy (review §2.1, decisions H1/H2/H4):
//! - `available`: always writable; fan-out is best-effort, ACKs are ignored.
//! - `quorum`: a write is acknowledged to the client only when at least
//!   `write_quorum` nodes durably hold it AT ACK TIME — the local copy plus
//!   every peer whose [`ClusterObjectAck`] certified row-applied AND
//!   blob-present. Two layers enforce this:
//!   1. an *admission gate* ([`ClusterState::has_write_quorum`]) refuses early
//!      (cheap fail-fast) when membership already knows too few nodes are live;
//!   2. *ACK counting* after the parallel fan-out closes the failure-detection
//!      window the gate cannot see (peers believed alive that did not ACK).
//!
//!   When the count falls short the client receives `503 ServiceUnavailable`
//!   (+ `Retry-After`), but the local copy is NOT rolled back — like any quorum
//!   system without distributed transactions, an error response means "not
//!   acknowledged as replicated", not "undone". Anti-entropy then either
//!   propagates the local copy (it survives) or a newer client retry overwrites
//!   it (LWW). Divergence inside the failure-detection window is therefore
//!   bounded to writes the client KNOWS were not acknowledged.
//!
//! ACK-counting scope (decision H4): object data-plane mutations only —
//! `put_object`, the delete marker, version hard-deletes. Control-plane and tag
//! ops remain best-effort fan-out + anti-entropy reconcile (rare mutations).
//!
//! Control plane replicated via `/cluster/v1/op` (`ControlOp`): bucket create /
//! delete, `bucket_config` (versioning, encryption, ...), bucket tags, object
//! tags, and in-progress multipart state (upload + part rows) — so a peer that
//! receives an object row, or that is load-balanced a later part / Complete for
//! an upload begun elsewhere, can serve and finish it. Retention and legal-hold
//! replicate by re-sending the mutated object row (the LWW `>=` guard applies an
//! equal-tuple row, carrying the updated lock columns).
//!
//! The IDENTITY control plane (credentials, users, teams, grants, server_config)
//! has its own store decorators in `cluster_control.rs` and reuses the same
//! `/cluster/v1/op` channel.
//!
//! `apply_remote_*` delegate straight to the inner store and never re-fan-out
//! (they apply rows already received from a peer).

use std::sync::Arc;

use arca_core::cluster::{quorum_satisfied, ClusterState, ControlOp, WriteGate};
use arca_core::error::ArcaError;
use arca_core::store::{ControlTombstoneStore, MetadataStore, TOMBSTONE_BUCKET};
use arca_core::types::{
    BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats,
};
use arca_core::{S3Error, S3ErrorCode};
use chrono::{DateTime, Utc};
use futures_util::future::join_all;

use crate::cluster::client::ClusterClient;

/// Maps the cluster admission gate to the client-facing `503` (shared by the
/// metadata and control-plane decorators). [`WriteGate::NoQuorum`] is the
/// fail-fast half of the §2.1 quorum (the ACK count is authoritative);
/// [`WriteGate::SizeExceeded`] is the H6 fail-closed guard against an
/// over-sized membership (D3a) — two distinct, actionable messages.
pub(crate) fn check_write_gate(cluster: &ClusterState) -> Result<(), ArcaError> {
    match cluster.write_gate() {
        WriteGate::Open => Ok(()),
        WriteGate::NoQuorum { eligible, quorum } => Err(ArcaError::S3(S3Error::with_message(
            S3ErrorCode::ServiceUnavailable,
            format!(
                "cluster write quorum not available ({eligible} eligible node(s), \
                 {quorum} required)"
            ),
            "/",
        ))),
        WriteGate::SizeExceeded {
            eligible,
            cluster_size,
        } => Err(ArcaError::S3(S3Error::with_message(
            S3ErrorCode::ServiceUnavailable,
            format!(
                "cluster size exceeded: {eligible} eligible nodes for cluster_size = \
                 {cluster_size} — writes are refused to prevent split-brain; remove the \
                 extra node(s) or resize the cluster (see the HA guide runbook)"
            ),
            "/",
        ))),
    }
}

/// Metadata store decorator that replicates object-table mutations to peers
/// under the configured consistency policy.
pub struct ClusterMetadataStore {
    inner: Arc<dyn MetadataStore>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
    tombstones: Arc<dyn ControlTombstoneStore>,
}

impl ClusterMetadataStore {
    pub fn new(
        inner: Arc<dyn MetadataStore>,
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

    /// Endpoints of peers eligible for replication: alive AND authenticated
    /// (proved possession of the cluster secret — decision H12) AND
    /// config-aligned (H7). Fanning out to anything less would hand new writes
    /// to a rogue mDNS registrant or to a node that cannot store them
    /// correctly (review §3.7(A), D1).
    fn live_peers(&self) -> Vec<String> {
        self.cluster
            .peers()
            .into_iter()
            .filter(|p| p.eligible())
            .map(|p| p.endpoint)
            .collect()
    }

    /// The consistency-policy admission gate. In `available` mode this is
    /// always `Ok`; in `quorum` mode it refuses with `503` when too few
    /// ELIGIBLE nodes are live, or — fail-closed, decision H6 — when MORE
    /// eligible nodes than `cluster_size` are live (split-brain enabler).
    /// Fail-fast only — the authoritative check is the post-fan-out ACK count
    /// ([`Self::enforce_ack_quorum`]), which sees what membership cannot.
    fn check_write_quorum(&self) -> Result<(), ArcaError> {
        check_write_gate(&self.cluster)
    }

    /// The durability quorum check (review §2.1, decision H1): `acks` counts
    /// the nodes durably holding the write (local + full peer ACKs). On a
    /// shortfall in quorum mode the client gets `503`; the local copy is NOT
    /// rolled back (anti-entropy propagates or LWW overwrites it — the error
    /// means "not acknowledged as replicated", not "undone").
    fn enforce_ack_quorum(&self, acks: usize) -> Result<(), ArcaError> {
        if quorum_satisfied(acks, self.cluster.write_quorum()) {
            Ok(())
        } else {
            Err(ArcaError::S3(S3Error::with_message(
                S3ErrorCode::ServiceUnavailable,
                format!(
                    "cluster write quorum not reached ({} of {} required copies acknowledged); \
                     the write is durable on this node and will reconcile, but it is not \
                     acknowledged as replicated — retry",
                    acks,
                    self.cluster.write_quorum().unwrap_or(1),
                ),
                "/",
            )))
        }
    }

    /// Replicates a fully-formed object row to every live peer IN PARALLEL
    /// (§2.4) and returns how many peers returned a FULL ack — row applied and
    /// referenced blob present (decision H2). Failures are logged and left to
    /// anti-entropy; the caller decides whether the count satisfies the quorum.
    async fn fan_out_object(&self, record: &ObjectRecord) -> usize {
        let sends = self.live_peers().into_iter().map(|endpoint| {
            let client = &self.client;
            async move {
                match client.send_object(&endpoint, record).await {
                    Ok(ack) => {
                        if !(ack.applied && ack.has_blob) {
                            tracing::warn!(
                                peer = %endpoint,
                                applied = ack.applied,
                                has_blob = ack.has_blob,
                                bucket = %record.bucket,
                                key = %record.key,
                                "cluster object fan-out: partial ack (will reconcile via anti-entropy)"
                            );
                        }
                        ack.applied && ack.has_blob
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            peer = %endpoint,
                            bucket = %record.bucket,
                            key = %record.key,
                            "cluster object fan-out failed (will reconcile via anti-entropy)"
                        );
                        false
                    }
                }
            }
        });
        join_all(sends).await.into_iter().filter(|ok| *ok).count()
    }

    /// Replicates a version hard-delete to every live peer IN PARALLEL (§2.4)
    /// and returns how many peers acknowledged it (no blob involved, so
    /// `applied` alone is a full ack — decision H2).
    async fn fan_out_version_delete(&self, bucket: &str, key: &str, version_id: &str) -> usize {
        let sends = self.live_peers().into_iter().map(|endpoint| {
            let client = &self.client;
            async move {
                match client
                    .send_version_delete(&endpoint, bucket, key, version_id)
                    .await
                {
                    Ok(ack) => ack.applied,
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            peer = %endpoint,
                            bucket = %bucket,
                            key = %key,
                            "cluster delete fan-out failed (will reconcile via anti-entropy)"
                        );
                        false
                    }
                }
            }
        });
        join_all(sends).await.into_iter().filter(|ok| *ok).count()
    }

    /// Re-sends the row whose lock columns (retention / legal-hold) just
    /// changed. The mutation leaves `(last_modified, version_id, blob_id)`
    /// unchanged, and `apply_remote_object`'s LWW guard accepts an equal tuple
    /// (`>=`), so peers adopt the new retention/legal-hold without minting a new
    /// version. `version_id == None` targets the current version. Best-effort
    /// (outside the H4 ACK-counting scope) — the ack count is ignored.
    async fn replicate_lock_change(&self, bucket: &str, key: &str, version_id: Option<&str>) {
        let row = match version_id {
            Some(vid) => self.inner.get_object_version(bucket, key, vid).await,
            None => self.inner.get_latest_object(bucket, key).await,
        };
        if let Ok(Some(record)) = row {
            let _acks = self.fan_out_object(&record).await;
        }
    }

    /// Replicates a control-plane op to every live peer IN PARALLEL (§2.4).
    /// Best-effort by design (decision H4): control-plane mutations are rare
    /// and reconciled by anti-entropy, so no ACK counting here.
    async fn fan_out_op(&self, op: &ControlOp) {
        let sends = self.live_peers().into_iter().map(|endpoint| {
            let client = &self.client;
            async move {
                if let Err(e) = client.send_op(&endpoint, op).await {
                    tracing::warn!(
                        error = %e,
                        peer = %endpoint,
                        "cluster control-plane fan-out failed (will reconcile via anti-entropy)"
                    );
                }
            }
        });
        join_all(sends).await;
    }
}

#[async_trait::async_trait]
impl MetadataStore for ClusterMetadataStore {
    // -- Bucket operations (delegate; control-plane replication is a follow-up) --

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, ArcaError> {
        self.inner.list_buckets().await
    }

    async fn create_bucket(&self, name: &str) -> Result<(), ArcaError> {
        self.check_write_quorum()?;
        self.inner.create_bucket(name).await?;
        // Clear any stale deletion tombstone so the reconcile does not later
        // re-delete this freshly (re-)created bucket.
        if let Err(e) = self
            .tombstones
            .delete_control_tombstone(TOMBSTONE_BUCKET, name)
            .await
        {
            tracing::warn!(error = %e, bucket = %name, "failed to clear stale bucket tombstone on create");
        }
        // Replicate the full row (created_at/owner) verbatim so peers can serve
        // objects written to this bucket.
        if let Ok(Some(info)) = self.inner.head_bucket(name).await {
            self.fan_out_op(&ControlOp::BucketUpsert { info }).await;
        }
        Ok(())
    }

    async fn head_bucket(&self, name: &str) -> Result<Option<BucketInfo>, ArcaError> {
        self.inner.head_bucket(name).await
    }

    async fn delete_bucket(&self, name: &str) -> Result<bool, ArcaError> {
        self.check_write_quorum()?;
        let existed = self.inner.delete_bucket(name).await?;
        if existed {
            // Record a deletion tombstone so the delete converges via reconcile
            // and is not resurrected by a peer that still holds the bucket row.
            if let Err(e) = self
                .tombstones
                .record_control_tombstone(TOMBSTONE_BUCKET, name)
                .await
            {
                tracing::warn!(error = %e, bucket = %name, "failed to record bucket tombstone (delete may be resurrected by reconcile)");
            }
            self.fan_out_op(&ControlOp::BucketDelete {
                name: name.to_string(),
            })
            .await;
        }
        Ok(existed)
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
        let acks = self.fan_out_object(&replicated).await;

        // True quorum (§2.1): local copy + full peer ACKs must reach the
        // threshold, else 503. On failure the local row (and any blob it
        // overwrote, now orphaned) stays — the anti-entropy GC reclaims
        // orphaned blobs and the manifest propagates the row.
        self.enforce_ack_quorum(1 + acks)?;

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
            // an unversioned bucket hard-deletes. Replicate whichever happened
            // and hold it to the same durability quorum as a put (§2.1/H4).
            let acks = match self.inner.get_latest_object(bucket, key).await {
                Ok(Some(marker)) if marker.is_delete_marker => {
                    self.fan_out_object(&marker).await
                }
                _ => {
                    let version_id = old
                        .as_ref()
                        .and_then(|o| o.version_id.clone())
                        .unwrap_or_else(|| "null".to_string());
                    self.fan_out_version_delete(bucket, key, &version_id).await
                }
            };
            self.enforce_ack_quorum(1 + acks)?;
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
            let acks = self.fan_out_version_delete(bucket, key, version_id).await;
            self.enforce_ack_quorum(1 + acks)?;
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

    // -- Multipart upload operations (in-progress state replicated) --

    async fn create_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        self.check_write_quorum()?;
        self.inner.create_multipart_upload(record).await?;
        self.fan_out_op(&ControlOp::MultipartCreate {
            record: record.clone(),
        })
        .await;
        Ok(())
    }

    async fn get_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Option<MultipartUploadRecord>, ArcaError> {
        self.inner.get_multipart_upload(upload_id).await
    }

    async fn put_part(&self, part: &PartRecord) -> Result<Option<PartRecord>, ArcaError> {
        self.check_write_quorum()?;
        let old = self.inner.put_part(part).await?;
        // The part blob itself already fanned out on its write_sidecar; this
        // replicates the part row so a peer can List/Complete the upload.
        self.fan_out_op(&ControlOp::PartUpsert { part: part.clone() })
            .await;
        Ok(old)
    }

    async fn list_parts(&self, upload_id: &str) -> Result<Vec<PartRecord>, ArcaError> {
        self.inner.list_parts(upload_id).await
    }

    async fn delete_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, ArcaError> {
        self.check_write_quorum()?;
        let parts = self.inner.delete_multipart_upload(upload_id).await?;
        self.fan_out_op(&ControlOp::MultipartDelete {
            upload_id: upload_id.to_string(),
        })
        .await;
        Ok(parts)
    }

    // -- Object Lock operations (replicated by re-sending the mutated row) --

    async fn set_object_retention(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        retention_mode: Option<&str>,
        retain_until_date: Option<&str>,
    ) -> Result<bool, ArcaError> {
        self.check_write_quorum()?;
        let changed = self
            .inner
            .set_object_retention(bucket, key, version_id, retention_mode, retain_until_date)
            .await?;
        if changed {
            self.replicate_lock_change(bucket, key, version_id).await;
        }
        Ok(changed)
    }

    async fn set_object_legal_hold(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool, ArcaError> {
        self.check_write_quorum()?;
        let changed = self
            .inner
            .set_object_legal_hold(bucket, key, version_id, status)
            .await?;
        if changed {
            self.replicate_lock_change(bucket, key, version_id).await;
        }
        Ok(changed)
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
        self.check_write_quorum()?;
        self.inner
            .set_bucket_config(bucket, config_key, config_value)
            .await?;
        self.fan_out_op(&ControlOp::BucketConfigSet {
            bucket: bucket.to_string(),
            key: config_key.to_string(),
            value: config_value.to_string(),
        })
        .await;
        Ok(())
    }

    async fn delete_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<bool, ArcaError> {
        self.check_write_quorum()?;
        let existed = self.inner.delete_bucket_config(bucket, config_key).await?;
        if existed {
            self.fan_out_op(&ControlOp::BucketConfigDelete {
                bucket: bucket.to_string(),
                key: config_key.to_string(),
            })
            .await;
        }
        Ok(existed)
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
        self.check_write_quorum()?;
        self.inner.put_bucket_tags(bucket, tags).await?;
        self.fan_out_op(&ControlOp::BucketTags {
            bucket: bucket.to_string(),
            tags: tags.to_vec(),
        })
        .await;
        Ok(())
    }

    async fn delete_bucket_tags(&self, bucket: &str) -> Result<bool, ArcaError> {
        self.check_write_quorum()?;
        let existed = self.inner.delete_bucket_tags(bucket).await?;
        if existed {
            // Replicate as a tags-replace with an empty set, clearing peers' tags.
            self.fan_out_op(&ControlOp::BucketTags {
                bucket: bucket.to_string(),
                tags: Vec::new(),
            })
            .await;
        }
        Ok(existed)
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
        self.check_write_quorum()?;
        self.inner
            .put_object_tags(bucket, key, version_id, tags)
            .await?;
        self.fan_out_op(&ControlOp::ObjectTags {
            bucket: bucket.to_string(),
            key: key.to_string(),
            version_id: version_id.to_string(),
            tags: tags.to_vec(),
        })
        .await;
        Ok(())
    }

    async fn delete_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<bool, ArcaError> {
        self.check_write_quorum()?;
        let existed = self.inner.delete_object_tags(bucket, key, version_id).await?;
        if existed {
            // Replicate as a replace with an empty set, clearing peers' tags.
            self.fan_out_op(&ControlOp::ObjectTags {
                bucket: bucket.to_string(),
                key: key.to_string(),
                version_id: version_id.to_string(),
                tags: Vec::new(),
            })
            .await;
        }
        Ok(existed)
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

    async fn apply_remote_bucket(&self, info: &BucketInfo) -> Result<(), ArcaError> {
        self.inner.apply_remote_bucket(info).await
    }

    async fn apply_remote_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        self.inner.apply_remote_multipart_upload(record).await
    }

    async fn list_rows_changed_since(
        &self,
        since: u64,
        limit: u32,
    ) -> Result<Vec<(u64, ObjectRecord)>, ArcaError> {
        // Read-only; the manifest endpoint serves it. No gate, no fan-out.
        self.inner.list_rows_changed_since(since, limit).await
    }

    async fn purge_tombstones(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, ArcaError> {
        // Local maintenance GC; no gate, no fan-out (each node GCs its own).
        self.inner.purge_tombstones(before).await
    }

    async fn current_object_seq(&self) -> Result<u64, ArcaError> {
        // Read-only; the ping endpoint reports it. No gate, no fan-out.
        self.inner.current_object_seq().await
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
            is_tombstone: false,
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
        ClusterClient::new("self-node", "secret", Duration::from_secs(1), None).unwrap()
    }

    /// A standalone in-memory tombstone store for decorator construction in
    /// tests (these gate tests do not assert tombstone recording).
    async fn tombstones() -> Arc<dyn ControlTombstoneStore> {
        Arc::new(arca_storage::SqliteStore::open_in_memory().await.unwrap())
    }

    fn live_peer() -> PeerNode {
        peer_at("http://127.0.0.1:1") // unreachable on purpose
    }

    fn peer_at(endpoint: &str) -> PeerNode {
        PeerNode {
            node_id: "peer-2".to_string(),
            endpoint: endpoint.to_string(),
            alive: true,
            last_seen: None,
            // These tests model an ELIGIBLE peer (membership authenticated it);
            // what they exercise is the fan-out/ACK behavior toward it.
            authenticated: true,
            config_ok: true,
            disk_total: None,
            disk_available: None,
        }
    }

    /// Spawns a minimal HTTP/1.1 peer answering every request `200 OK` with the
    /// given JSON body — enough for the reqwest-based fan-out to parse an ack.
    /// The listener dies with the returned handle.
    async fn spawn_fake_peer(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    // Read the full request (headers + content-length body).
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let (mut content_len, mut header_end) = (0usize, 0usize);
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if header_end == 0 {
                            if let Some(pos) =
                                buf.windows(4).position(|w| w == b"\r\n\r\n")
                            {
                                header_end = pos + 4;
                                let headers =
                                    String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
                                content_len = headers
                                    .lines()
                                    .find_map(|l| l.strip_prefix("content-length:"))
                                    .and_then(|v| v.trim().parse().ok())
                                    .unwrap_or(0);
                            }
                        }
                        if header_end > 0 && buf.len() >= header_end + content_len {
                            break;
                        }
                    }
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn quorum_mode_refuses_write_without_majority() {
        let (inner, _dir) = temp_store().await;
        // cluster_size=3 -> write_quorum=2; alone (no live peers) -> 1 < 2 -> read-only.
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
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
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        store.put_object(&sample_record()).await.unwrap();
        assert!(store.get_object("b", "k").await.unwrap().is_some());
    }

    /// Review §2.1 (P0): the admission gate alone is NOT a quorum. A peer that
    /// membership still believes alive but that does not ACK the fan-out must
    /// fail the write — otherwise an acknowledged PUT exists on one machine
    /// only (a ghost write). This pins the true-quorum semantics (H1/H2).
    #[tokio::test]
    async fn quorum_mode_refuses_write_when_acks_below_quorum() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        cluster.set_peers(vec![live_peer()]); // gate sees 2 "alive" -> passes
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        // The unreachable peer never ACKs: 1 (local) < 2 -> 503.
        let err = store.put_object(&sample_record()).await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
        // No rollback (H1): the local copy stays for anti-entropy to propagate.
        assert!(store.get_object("b", "k").await.unwrap().is_some());
    }

    /// The happy path of the true quorum: a peer that fully ACKs (row applied +
    /// blob present) makes 1 + 1 = 2 >= 2 and the write succeeds.
    #[tokio::test]
    async fn quorum_mode_writes_when_peer_acks() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let (endpoint, _peer) =
            spawn_fake_peer(r#"{"applied":true,"has_blob":true}"#).await;
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        cluster.set_peers(vec![peer_at(&endpoint)]);
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        store.put_object(&sample_record()).await.unwrap();
        assert!(store.get_object("b", "k").await.unwrap().is_some());
    }

    /// A peer that applied the row but is missing the blob is NOT a durable
    /// copy: its ack must not count toward the quorum (decision H2).
    #[tokio::test]
    async fn quorum_mode_partial_ack_does_not_count() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let (endpoint, _peer) =
            spawn_fake_peer(r#"{"applied":true,"has_blob":false}"#).await;
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        cluster.set_peers(vec![peer_at(&endpoint)]);
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        let err = store.put_object(&sample_record()).await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    /// A legacy peer (pre-ACK wire format) answers 200 with an empty body; the
    /// rolling-upgrade contract (H10) counts it as a full ack.
    #[tokio::test]
    async fn quorum_mode_legacy_empty_ack_counts() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let (endpoint, _peer) = spawn_fake_peer("").await;
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        cluster.set_peers(vec![peer_at(&endpoint)]);
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        store.put_object(&sample_record()).await.unwrap();
    }

    /// Deletes are held to the same quorum as puts (H4): an unreachable peer
    /// fails the delete with 503, and the local delete is NOT rolled back.
    #[tokio::test]
    async fn quorum_mode_refuses_delete_when_acks_below_quorum() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        cluster.set_peers(vec![live_peer()]);
        let store = ClusterMetadataStore::new(
            inner.clone(),
            client(),
            cluster.clone(),
            tombstones().await,
        );
        // Seed the object with the peer "ACKing" is impossible here (peer is
        // unreachable), so write it via the inner store directly.
        inner.put_object(&sample_record()).await.unwrap();
        let err = store.delete_object("b", "k").await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
        // No rollback: locally the object is gone; the deletion will reconcile.
        assert!(store.get_object("b", "k").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn quorum_mode_refuses_create_bucket_without_majority() {
        let (inner, _dir) = temp_store().await;
        // cluster_size=3 -> quorum=2; alone -> control-plane writes are refused too.
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        let err = store.create_bucket("b").await.unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn available_mode_create_bucket_alone() {
        let (inner, _dir) = temp_store().await;
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        // available mode: bucket creation succeeds solo; no peers -> no fan-out.
        store.create_bucket("b").await.unwrap();
        assert!(store.head_bucket("b").await.unwrap().is_some());
    }

    fn sample_upload() -> MultipartUploadRecord {
        MultipartUploadRecord {
            upload_id: "u1".to_string(),
            bucket: "b".to_string(),
            key: "k".to_string(),
            content_type: None,
            initiated_at: Utc::now(),
            metadata: Default::default(),
            checksum_algorithm: None,
        }
    }

    #[tokio::test]
    async fn available_mode_multipart_and_tags_alone() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);

        // Multipart in-progress state: create -> put_part -> delete, all solo.
        store.create_multipart_upload(&sample_upload()).await.unwrap();
        assert!(store.get_multipart_upload("u1").await.unwrap().is_some());
        let part = PartRecord {
            upload_id: "u1".to_string(),
            part_number: 1,
            blob_id: BlobId("part-blob".to_string()),
            size: 4,
            etag: "e".to_string(),
            checksum_value: None,
            last_modified: None,
        };
        store.put_part(&part).await.unwrap();
        assert_eq!(store.list_parts("u1").await.unwrap().len(), 1);
        store.delete_multipart_upload("u1").await.unwrap();
        assert!(store.get_multipart_upload("u1").await.unwrap().is_none());

        // Object tags: put then delete, solo.
        store.put_object(&sample_record()).await.unwrap();
        store
            .put_object_tags("b", "k", "", &[("env".to_string(), "prod".to_string())])
            .await
            .unwrap();
        assert_eq!(store.get_object_tags("b", "k", "").await.unwrap().len(), 1);
        assert!(store.delete_object_tags("b", "k", "").await.unwrap());
        assert!(store.get_object_tags("b", "k", "").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn quorum_mode_refuses_multipart_create_without_majority() {
        let (inner, _dir) = temp_store().await;
        let cluster = Arc::new(ClusterState::new("self-node", Some(2), Some(3)));
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        let err = store
            .create_multipart_upload(&sample_upload())
            .await
            .unwrap_err();
        match err {
            ArcaError::S3(e) => assert_eq!(e.code, S3ErrorCode::ServiceUnavailable),
            other => panic!("expected ServiceUnavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn available_mode_set_retention_alone() {
        let (inner, _dir) = temp_store().await;
        inner.create_bucket("b").await.unwrap();
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        let store = ClusterMetadataStore::new(inner, client(), cluster, tombstones().await);
        store.put_object(&sample_record()).await.unwrap();
        // Sets the lock columns on the current version; replication re-sends the
        // row (no peers here, so just verify the local write + read-back path).
        let changed = store
            .set_object_retention("b", "k", None, Some("GOVERNANCE"), Some("2099-01-01T00:00:00Z"))
            .await
            .unwrap();
        assert!(changed);
        let row = store.get_latest_object("b", "k").await.unwrap().unwrap();
        assert_eq!(row.retention_mode.as_deref(), Some("GOVERNANCE"));
    }
}
