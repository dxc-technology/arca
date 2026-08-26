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
    SidecarMeta, SsecBlobOps,
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
        repair_from_peers(self.raw.as_ref(), &self.client, &self.live_peers(), blob_id).await
    }
}

/// Fetches a locally-missing blob (bytes + sidecar, verbatim) from the first
/// eligible peer that has it. Shared by the [`ClusterBlobStore`] read-repair
/// and the SSE-C decorator below.
async fn repair_from_peers(
    raw: &dyn RawBlobOps,
    client: &ClusterClient,
    peers: &[String],
    blob_id: &BlobId,
) -> bool {
    for endpoint in peers {
        match client.fetch_blob(endpoint, blob_id).await {
            Ok((sidecar, stream)) => {
                // Composite blobs carry no bytes; only the sidecar is written
                // (its parts repair on their own GETs).
                if sidecar.composite.is_none() {
                    if let Err(e) = raw.write_raw(blob_id, stream).await {
                        tracing::warn!(error = %e, "cluster read-repair: write_raw failed");
                        continue;
                    }
                }
                if let Err(e) = raw.write_sidecar(blob_id, &sidecar).await {
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

/// SSE-C blob-ops decorator (§3.6). SSE-C blobs already REPLICATE like any
/// other blob: the handler writes the object sidecar through the
/// cluster-wrapped `BlobStore`, whose fan-out ships the on-disk (customer-key
/// encrypted) bytes verbatim — peers store them without ever seeing the key.
/// What the unwrapped path lacked was the read side: `get_with_key` opens the
/// blob file directly, so a node that received the object row but not yet the
/// bytes (failed fan-out, partition) returned an error until the next
/// anti-entropy repair pass. This decorator closes that window with the same
/// synchronous read-repair the plain `get` path has.
pub struct ClusterSsecBlobStore {
    inner: Arc<dyn SsecBlobOps>,
    raw: Arc<dyn RawBlobOps>,
    client: ClusterClient,
    cluster: Arc<ClusterState>,
}

impl ClusterSsecBlobStore {
    pub fn new(
        inner: Arc<dyn SsecBlobOps>,
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

    fn live_peers(&self) -> Vec<String> {
        self.cluster
            .peers()
            .into_iter()
            .filter(|p| p.eligible())
            .map(|p| p.endpoint)
            .collect()
    }
}

#[async_trait::async_trait]
impl SsecBlobOps for ClusterSsecBlobStore {
    async fn put_with_key(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
        customer_key: &[u8; 32],
    ) -> Result<(BlobPutResult, [u8; 4]), ArcaError> {
        // Local write; replication happens on the handler's subsequent
        // write_sidecar through the cluster-wrapped BlobStore (same order as
        // every other blob).
        self.inner.put_with_key(blob_id, stream, customer_key).await
    }

    async fn get_with_key(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
        customer_key: &[u8; 32],
        nonce_prefix: &[u8; 4],
        plaintext_size: u64,
    ) -> Result<BlobGetResult, ArcaError> {
        // Same read-repair contract as ClusterBlobStore::get: a missing
        // sidecar means the bytes never arrived here — fetch them (still
        // encrypted with the customer key) from a peer, then decrypt locally.
        if let Ok(None) = self.raw.read_sidecar(blob_id).await {
            repair_from_peers(self.raw.as_ref(), &self.client, &self.live_peers(), blob_id)
                .await;
        }
        self.inner
            .get_with_key(blob_id, range, customer_key, nonce_prefix, plaintext_size)
            .await
    }

    async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        // Local only — peers reclaim the orphaned blob via anti-entropy GC.
        self.inner.delete(blob_id).await
    }

    async fn write_sidecar(&self, blob_id: &BlobId, meta: &SidecarMeta) -> Result<(), ArcaError> {
        self.inner.write_sidecar(blob_id, meta).await
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

    async fn delete_assembled(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        // Local only, same as delete() — no cascade into composite parts.
        self.inner.delete_assembled(blob_id).await
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
        // CompleteMultipartUpload may land on a node that learned the part
        // ROWS via replication but never received some part BYTES (their
        // blob fan-out failed, or the upload spanned a partition). Repair the
        // missing parts from peers BEFORE assembling — the inner concat needs
        // every part sidecar (and, on the byte-copy fallback, the bytes) to
        // be present locally (D4).
        for part_id in part_blob_ids {
            match self.raw.read_sidecar(part_id).await {
                Ok(None) => {
                    if !self.try_repair(part_id).await {
                        tracing::warn!(
                            blob_id = %part_id.0,
                            "cluster concat: part missing locally and no peer supplied it"
                        );
                    }
                }
                // Present, or probe failed: let the inner concat surface it.
                Ok(Some(_)) | Err(_) => {}
            }
        }
        // Local concat; the composite sidecar's write_sidecar replicates it.
        self.inner.concat(part_blob_ids, output_blob_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::cluster::{ClusterState, PeerNode, CLUSTER_SIDECAR_HEADER};
    use base64::engine::general_purpose::STANDARD as BASE64;
    use base64::Engine as _;
    use bytes::Bytes;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_stream::iter as stream_iter;

    fn bytes_to_stream(data: &[u8]) -> ByteStream {
        Box::pin(stream_iter(vec![Ok(Bytes::copy_from_slice(data))]))
    }

    async fn fs_store() -> (Arc<arca_storage::FsBlobStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = arca_storage::FsBlobStore::new(dir.path().join("blobs"), 2)
            .await
            .unwrap();
        (Arc::new(store), dir)
    }

    fn client() -> ClusterClient {
        ClusterClient::new("self-node", "secret", Duration::from_secs(1), None).unwrap()
    }

    fn peer_at(endpoint: &str) -> PeerNode {
        PeerNode {
            node_id: "peer-2".to_string(),
            endpoint: endpoint.to_string(),
            alive: true,
            last_seen: None,
            authenticated: true,
            config_ok: true,
            disk_total: None,
            disk_available: None,
            max_seq: None,
        }
    }

    fn part_sidecar(key: &str, size: u64, etag: &str) -> SidecarMeta {
        SidecarMeta {
            bucket: "b".to_string(),
            key: key.to_string(),
            size,
            etag: etag.to_string(),
            content_type: None,
            last_modified: chrono::Utc::now().to_rfc3339(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    /// Collects a ByteStream into owned bytes.
    async fn collect(mut stream: ByteStream) -> Vec<u8> {
        use futures_util::StreamExt;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    /// Minimal HTTP/1.1 peer answering every GET with `200 OK`, the given
    /// sidecar in the [`CLUSTER_SIDECAR_HEADER`] response header and the raw
    /// bytes as body — what `ClusterClient::fetch_blob` consumes for repair.
    async fn spawn_blob_peer(
        sidecar: SidecarMeta,
        body: Vec<u8>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sidecar_b64 = BASE64.encode(serde_json::to_vec(&sidecar).unwrap());
        let body = std::sync::Arc::new(body);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let sidecar_b64 = sidecar_b64.clone();
                let body = body.clone();
                tokio::spawn(async move {
                    // Drain the request head (GETs carry no body).
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/octet-stream\r\n\
                         {CLUSTER_SIDECAR_HEADER}: {sidecar_b64}\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len(),
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.write_all(&body).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    /// D4: CompleteMultipartUpload on a node that has the part ROWS but is
    /// missing some part BYTES must fetch them from a peer before assembling,
    /// instead of failing the concat.
    #[tokio::test]
    async fn concat_repairs_missing_part_from_peer() {
        let (fs, _dir) = fs_store().await;

        // Part 1 written locally (blob + sidecar, the normal UploadPart order).
        let p1 = BlobId::new();
        let r1 = fs.put(&p1, bytes_to_stream(b"hello ")).await.unwrap();
        RawBlobOps::write_sidecar(fs.as_ref(), &p1, &part_sidecar("k#up#1", r1.size, &r1.etag))
            .await
            .unwrap();

        // Part 2 exists only on the fake peer — written through a real store
        // there, so its sidecar carries a genuine MD5 etag (the composite fast
        // path decodes part etags; a non-hex etag would force the byte-copy
        // fallback and mask what this test pins).
        let (peer_fs, _dp) = fs_store().await;
        let p2 = BlobId::new();
        let r2 = peer_fs.put(&p2, bytes_to_stream(b"world")).await.unwrap();
        let bytes2 = collect(peer_fs.read_raw(&p2).await.unwrap().stream).await;
        let (endpoint, _peer) =
            spawn_blob_peer(part_sidecar("k#up#2", r2.size, &r2.etag), bytes2).await;

        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        cluster.set_peers(vec![peer_at(&endpoint)]);
        let store = ClusterBlobStore::new(fs.clone(), fs.clone(), client(), cluster);

        let out = BlobId::new();
        let result = store.concat(&[p1.clone(), p2.clone()], &out).await.unwrap();

        // The missing part was repaired locally (sidecar + bytes)...
        assert!(fs.read_sidecar(&p2).await.unwrap().is_some(), "part repaired");
        // ...and the concat assembled the composite over both parts.
        let parts = result.composite_parts.expect("composite fast path");
        assert_eq!(parts.len(), 2);
    }

    /// §3.6: an SSE-C GET on a node that has the object row but not (yet) the
    /// bytes must repair them from a peer — verbatim, still encrypted with the
    /// customer key — and decrypt locally with the caller's key.
    #[tokio::test]
    async fn ssec_get_repairs_missing_blob_from_peer() {
        use arca_core::store::BlobEncryptionInfo;

        // Origin node: encrypt with the customer key, capture the on-disk
        // ciphertext (exactly what a fan-out / repair ships).
        let (origin_fs, _d1) = fs_store().await;
        let origin_ssec = arca_storage::SsecBlobStore::new((*origin_fs).clone());
        let blob_id = BlobId::new();
        let key = [7u8; 32];
        let data = b"ssec secret payload";
        let (put, nonce_prefix) = origin_ssec
            .put_with_key(&blob_id, bytes_to_stream(data), &key)
            .await
            .unwrap();
        let cipher = collect(origin_fs.read_raw(&blob_id).await.unwrap().stream).await;
        assert_ne!(cipher, data.to_vec(), "stored bytes are encrypted");

        let mut sidecar = part_sidecar("k", put.size, &put.etag);
        sidecar.encryption = Some(BlobEncryptionInfo {
            algorithm: "SSE-C".to_string(),
            encrypted_dek: String::new(),
            dek_nonce: String::new(),
            nonce_prefix: BASE64.encode(nonce_prefix),
            key_id: String::new(),
        });
        let (endpoint, _peer) = spawn_blob_peer(sidecar, cipher).await;

        // Local node: empty disk, cluster-wrapped SSE-C ops.
        let (local_fs, _d2) = fs_store().await;
        let local_ssec: Arc<dyn SsecBlobOps> =
            Arc::new(arca_storage::SsecBlobStore::new((*local_fs).clone()));
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        cluster.set_peers(vec![peer_at(&endpoint)]);
        let store =
            ClusterSsecBlobStore::new(local_ssec, local_fs.clone(), client(), cluster);

        let got = store
            .get_with_key(&blob_id, None, &key, &nonce_prefix, data.len() as u64)
            .await
            .unwrap();
        assert_eq!(collect(got.stream).await, data.to_vec());
        // The repaired bytes are durable locally for subsequent reads.
        assert!(local_fs.read_sidecar(&blob_id).await.unwrap().is_some());
    }

    /// With every part already local the pre-check is a pure no-op pass-through.
    #[tokio::test]
    async fn concat_with_all_parts_local_needs_no_peer() {
        let (fs, _dir) = fs_store().await;
        let mut ids = Vec::new();
        for (i, data) in [b"foo".as_slice(), b"bar".as_slice()].iter().enumerate() {
            let id = BlobId::new();
            let r = fs.put(&id, bytes_to_stream(data)).await.unwrap();
            RawBlobOps::write_sidecar(
                fs.as_ref(),
                &id,
                &part_sidecar(&format!("k#up#{i}"), r.size, &r.etag),
            )
            .await
            .unwrap();
            ids.push(id);
        }
        // No peers at all: the concat must not need any.
        let cluster = Arc::new(ClusterState::new("self-node", None, None));
        let store = ClusterBlobStore::new(fs.clone(), fs.clone(), client(), cluster);
        let out = BlobId::new();
        let result = store.concat(&ids, &out).await.unwrap();
        assert!(result.composite_parts.is_some());
    }
}
