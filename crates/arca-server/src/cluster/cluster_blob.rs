//! Cluster blob store decorator (Phase 29 M3 — data-plane write path).
//!
//! Wraps the local blob store and replicates blobs to peers, keeping the
//! `BlobStore` trait so handlers and `AppState` are unchanged. It sits at the
//! top of the blob stack (above compression/encryption) and replicates the
//! already-encoded on-disk bytes verbatim via [`ClusterClient`].
//!
//! - **Fan-out on `write_sidecar`**: by the time the sidecar is written the
//!   blob bytes are durable on disk, so this is the single point where both the
//!   raw bytes and the sidecar are available to ship to peers. `put` /
//!   `put_with_hints` / `concat` only write locally; the subsequent
//!   `write_sidecar` does the replication.
//! - **Read-repair on `get`**: if the blob is absent locally (no sidecar) — the
//!   typical "peer received the object row but not the blob" case — fetch it
//!   from a live peer, store it, then serve.
//! - **`delete` is local-only**: orphaned blobs on peers are reclaimed by the
//!   anti-entropy GC (M4), not by an explicit cross-node blob delete.
//!
//! Fan-out is best-effort: peers that are unreachable now are reconciled by
//! hinted-handoff / anti-entropy (M4). Today a failed fan-out is logged.

use std::sync::Arc;

use arca_core::cluster::ClusterState;
use arca_core::error::ArcaError;
use arca_core::store::{
    BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream, PutHints, RawBlobOps,
    SidecarMeta,
};
use arca_core::types::BlobId;

use crate::cluster::client::ClusterClient;

/// Blob store decorator that replicates writes to cluster peers and repairs
/// local misses by fetching from peers.
pub struct ClusterBlobStore {
    /// Local blob stack (compression/encryption/fs) for normal reads/writes.
    inner: Arc<dyn BlobStore>,
    /// Raw verbatim access to the physical files, for replicating/repairing
    /// the already-encoded bytes (bypasses the wrappers).
    raw: Arc<dyn RawBlobOps>,
    /// Signed transport to peers.
    client: ClusterClient,
    /// Shared peer/liveness view.
    cluster: Arc<ClusterState>,
}

impl ClusterBlobStore {
    pub fn new(
        inner: Arc<dyn BlobStore>,
        raw: Arc<dyn RawBlobOps>,
        client: ClusterClient,
        cluster: Arc<ClusterState>,
    ) -> Self {
        Self {
            inner,
            raw,
            client,
            cluster,
        }
    }

    /// Endpoints of peers eligible for replication (membership already
    /// excludes this node): alive AND authenticated (proved possession of the
    /// cluster secret — decision H12) AND config-aligned (H7). Blob bytes are
    /// never shipped to — nor repaired from — a peer that has not proven
    /// itself (review §3.7(A)).
    fn live_peers(&self) -> Vec<String> {
        self.cluster
            .peers()
            .into_iter()
            .filter(|p| p.eligible())
            .map(|p| p.endpoint)
            .collect()
    }

    /// Replicates a blob (raw bytes + sidecar) to every live peer IN PARALLEL
    /// (§2.4). Best-effort: failures are logged and left for anti-entropy to
    /// reconcile — durability is accounted for at the object-row fan-out, where
    /// each peer self-certifies blob presence in its ack (decision H2), so a
    /// peer this fan-out missed simply cannot contribute to the write quorum.
    async fn fan_out(&self, blob_id: &BlobId, meta: &SidecarMeta) {
        let sends = self.live_peers().into_iter().map(|endpoint| async move {
            // Composite blobs have no physical file: ship the sidecar only.
            // Each send opens its own read of the raw bytes (a ByteStream is
            // not cloneable; the OS page cache makes the re-reads cheap).
            let body: ByteStream = if meta.composite.is_some() {
                Box::pin(futures_util::stream::empty::<
                    Result<bytes::Bytes, std::io::Error>,
                >())
            } else {
                match self.raw.read_raw(blob_id).await {
                    Ok(r) => r.stream,
                    Err(e) => {
                        tracing::warn!(error = %e, blob_id = %blob_id.0, "cluster fan-out: read_raw failed");
                        return;
                    }
                }
            };
            if let Err(e) = self.client.send_blob(&endpoint, blob_id, meta, body).await {
                tracing::warn!(
                    error = %e,
                    peer = %endpoint,
                    blob_id = %blob_id.0,
                    "cluster blob fan-out failed (will reconcile via anti-entropy)"
                );
            }
        });
        futures_util::future::join_all(sends).await;
    }

    /// Attempts to repair a locally-missing blob by fetching it from a live
    /// peer and storing it verbatim. Returns whether a peer supplied it.
    async fn try_repair(&self, blob_id: &BlobId) -> bool {
        for endpoint in self.live_peers() {
            match self.client.fetch_blob(&endpoint, blob_id).await {
                Ok((sidecar, stream)) => {
                    // Composite blobs carry no bytes; only the sidecar is written
                    // (its parts repair on their own GETs).
                    if sidecar.composite.is_none() {
                        if let Err(e) = self.raw.write_raw(blob_id, stream).await {
                            tracing::warn!(error = %e, "cluster read-repair: write_raw failed");
                            continue;
                        }
                    }
                    if let Err(e) = self.raw.write_sidecar(blob_id, &sidecar).await {
                        tracing::warn!(error = %e, "cluster read-repair: write_sidecar failed");
                        continue;
                    }
                    tracing::info!(blob_id = %blob_id.0, peer = %endpoint, "cluster read-repair succeeded");
                    return true;
                }
                Err(_) => continue,
            }
        }
        false
    }
}

#[async_trait::async_trait]
impl BlobStore for ClusterBlobStore {
    async fn put(&self, blob_id: &BlobId, stream: ByteStream) -> Result<BlobPutResult, ArcaError> {
        // Write locally; replication happens on the subsequent write_sidecar,
        // where both the bytes and the sidecar are available.
        self.inner.put(blob_id, stream).await
    }

    async fn put_with_hints(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
        hints: PutHints,
    ) -> Result<BlobPutResult, ArcaError> {
        self.inner.put_with_hints(blob_id, stream, hints).await
    }

    async fn get(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        // A present sidecar means the blob is here (normal file or composite).
        // Only when it is absent do we attempt a read-repair from a peer.
        match self.raw.read_sidecar(blob_id).await {
            Ok(None) => {
                self.try_repair(blob_id).await;
                self.inner.get(blob_id, range).await
            }
            // Present, or sidecar probe failed: let the inner store handle it.
            Ok(Some(_)) | Err(_) => self.inner.get(blob_id, range).await,
        }
    }

    async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        // Local only — peers reclaim the orphaned blob via anti-entropy GC (M4).
        self.inner.delete(blob_id).await
    }

    async fn write_sidecar(&self, blob_id: &BlobId, meta: &SidecarMeta) -> Result<(), ArcaError> {
        // Durable locally first (preserves the blob -> sidecar -> metadata order),
        // then replicate the bytes + sidecar to peers.
        self.inner.write_sidecar(blob_id, meta).await?;
        self.fan_out(blob_id, meta).await;
        Ok(())
    }

    async fn concat(
        &self,
        part_blob_ids: &[BlobId],
        output_blob_id: &BlobId,
    ) -> Result<BlobPutResult, ArcaError> {
        // Local concat; the composite sidecar's write_sidecar replicates it.
        self.inner.concat(part_blob_ids, output_blob_id).await
    }
}
