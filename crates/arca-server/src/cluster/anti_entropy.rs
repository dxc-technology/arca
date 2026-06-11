//! Cluster anti-entropy worker (Phase 29 M4) — the self-heal.
//!
//! Periodically reconciles this node with its peers so a node that was down,
//! lagging, or that missed a real-time fan-out catches up on its own. This is
//! the mechanism that makes a returning node converge without operator action.
//!
//! - **Objects**: for each live peer, pull its changed-since manifest
//!   (`seq > high-water-mark`) and apply every row via
//!   [`MetadataStore::apply_remote_object`] (idempotent, last-writer-wins).
//!   Tombstones are ordinary rows in the manifest, so deletions converge too
//!   and are never resurrected.
//! - **Control plane**: for each live peer, pull its full
//!   [`ControlSnapshot`] and merge it last-writer-wins
//!   ([`plan_control_merge`]) — credentials, users, teams, grants, buckets, with
//!   deletions carried as tombstones so a peer that still holds a deleted entity
//!   cannot resurrect it. The control plane is small, so shipping the whole
//!   snapshot each pass is cheap and also bootstraps a long-absent node past
//!   tombstone GC.
//! - **Tombstone GC**: drop object AND control tombstones older than the
//!   configured grace window (which must exceed the longest expected node
//!   downtime). Guarded by liveness (§3.2): while any known peer has been
//!   unseen beyond the grace, the purge is skipped (warn + the
//!   `tombstone_gc_blocked` flag in `/admin/cluster`) so the returning peer
//!   still finds the tombstones; membership pruning (M3) eventually evicts a
//!   never-returning peer and unblocks the GC.
//! - **Blobs** (slower cadence): proactively REPAIR blob bytes missing for local
//!   object rows (fetch from a peer), then GC orphan blob files no live row /
//!   in-progress part / non-orphan composite sidecar references and older than
//!   the grace. The GC is composite-aware (a composite's parts are kept alive by
//!   its sidecar) and fail-safe (any enumeration error skips the pass).
//!
//! The changed-since manifest is incremental and indexed by `seq`, so this can
//! run frequently and cheaply — that is why Arca relies on frequent anti-entropy
//! plus read-repair instead of the hinted-handoff machinery large clusters need
//! (their reconciliation is expensive and runs rarely).
//!
//! The high-water mark is per-peer and **in-memory**: this node's view of how
//! far it has consumed each peer's `seq`. It is node-local and must never be
//! replicated (it is meaningless elsewhere). On restart it resets to 0, costing
//! one extra full manifest pass per peer — idempotent, then incremental.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arca_core::cluster::{
    plan_blob_gc, plan_control_merge, tombstone_gc_blockers, ClusterState, ManifestEntry,
};
use arca_core::store::{ControlSnapshotStore, ControlTombstoneStore, MetadataStore, RawBlobOps};
use arca_core::types::BlobId;
use chrono::Utc;

use crate::cluster::client::{ClusterClient, ClusterError};
use crate::worker::BackgroundWorker;

/// Object manifest page size per request (the server clamps to its own max).
const MANIFEST_BATCH: u32 = 500;

/// Run the blob scan (proactive repair + orphan GC) every N anti-entropy ticks.
/// It is an O(blobs) sweep, so it runs on a slower cadence than the cheap
/// incremental object/control reconcile (lazy read-repair on GET still covers
/// on-access correctness between sweeps).
const BLOB_SCAN_EVERY_TICKS: u64 = 10;

/// Spawns the anti-entropy worker. It runs for the lifetime of the returned
/// handle, which the caller keeps alive.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    cluster: Arc<ClusterState>,
    client: ClusterClient,
    metadata: Arc<dyn MetadataStore>,
    raw: Arc<dyn RawBlobOps>,
    control_snapshot: Arc<dyn ControlSnapshotStore>,
    control_tombstone: Arc<dyn ControlTombstoneStore>,
    interval: Duration,
    tombstone_grace: Duration,
) -> BackgroundWorker {
    let handle = tokio::spawn(async move {
        // Per-peer high-water mark: highest seq applied from each peer node.
        let mut hwm: HashMap<String, u64> = HashMap::new();
        let mut timer = tokio::time::interval(interval.max(Duration::from_secs(1)));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        timer.tick().await; // skip the immediate first tick
        let mut tick: u64 = 0;

        loop {
            timer.tick().await;
            tick += 1;

            // Reconcile only with ELIGIBLE peers (alive + authenticated +
            // config-aligned, decision H12): pulling a manifest or a control
            // snapshot from a peer that never proved possession of the secret
            // would let a rogue endpoint feed us fabricated rows (data
            // poisoning via LWW), and blobs repaired from a wrong-master-key
            // node would be undecryptable here.
            for peer in cluster.peers().into_iter().filter(|p| p.eligible()) {
                // 1) Objects: pull this peer's changed-since manifest.
                let since = hwm.get(&peer.node_id).copied().unwrap_or(0);
                match reconcile_peer_objects(&client, metadata.as_ref(), &peer.endpoint, since)
                    .await
                {
                    Ok(cursor) => {
                        hwm.insert(peer.node_id, cursor);
                    }
                    Err(e) => {
                        tracing::debug!(
                            peer = %peer.endpoint,
                            error = %e,
                            "anti-entropy: object reconcile failed (retried next tick)"
                        );
                    }
                }

                // 2) Control plane: pull this peer's snapshot and merge LWW.
                if let Err(e) = reconcile_peer_control(
                    &client,
                    control_snapshot.as_ref(),
                    metadata.as_ref(),
                    &peer.endpoint,
                )
                .await
                {
                    tracing::debug!(
                        peer = %peer.endpoint,
                        error = %e,
                        "anti-entropy: control reconcile failed (retried next tick)"
                    );
                }
            }

            // 3) Tombstone GC: drop object AND control tombstones past the grace
            // — but ONLY while every known peer has been seen within it (§3.2
            // liveness guard). A tombstone purged while a peer is unreachable
            // beyond the grace would be gone before that peer ever learns of
            // the deletion; its stale rows would resurrect the data on
            // re-entry. Membership pruning (M3) eventually removes a
            // never-returning peer so it cannot block GC forever.
            let grace = chrono::Duration::from_std(tombstone_grace)
                .unwrap_or_else(|_| chrono::Duration::days(7));
            let blockers = tombstone_gc_blockers(&cluster.peers(), Utc::now(), grace);
            cluster.set_tombstone_gc_blocked(!blockers.is_empty());
            if !blockers.is_empty() {
                let who: Vec<String> = blockers
                    .iter()
                    .map(|p| format!("{} ({})", p.node_id, p.endpoint))
                    .collect();
                tracing::warn!(
                    blockers = %who.join(", "),
                    "anti-entropy: SKIPPING tombstone GC — peer(s) unseen beyond the \
                     tombstone grace window. Deleted data is kept as tombstones so it \
                     cannot resurrect when they return. Recover or remove the peer(s); \
                     membership pruning will eventually evict them (peer_prune_days)."
                );
            } else {
                let cutoff = Utc::now() - grace;
                match metadata.purge_tombstones(cutoff).await {
                    Ok(n) if n > 0 => {
                        tracing::debug!(purged = n, "anti-entropy: object tombstone GC")
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "anti-entropy: object tombstone GC failed")
                    }
                }
                match control_tombstone.purge_control_tombstones(cutoff).await {
                    Ok(n) if n > 0 => {
                        tracing::debug!(purged = n, "anti-entropy: control tombstone GC")
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, "anti-entropy: control tombstone GC failed")
                    }
                }
            }

            // 4) Blob scan (slower cadence): proactively fetch bytes for rows
            // whose blob is missing locally (so durability does not wait for a
            // GET to trigger the lazy read-repair), then reclaim orphan blobs.
            if tick % BLOB_SCAN_EVERY_TICKS == 0 {
                repair_blobs(&client, metadata.as_ref(), raw.as_ref(), &cluster).await;
                // Reuse the tombstone grace: like a tombstone, an orphan blob
                // must outlive the max reconcile lag before it is safe to reclaim.
                gc_blobs(metadata.as_ref(), raw.as_ref(), tombstone_grace).await;
            }
        }
    });
    BackgroundWorker::from_handle(handle)
}

/// Proactively repairs locally-missing blob bytes: for every blob_id referenced
/// by metadata, if the physical file is absent, fetch it (or, for a composite,
/// its missing parts) from a live peer. Bounded by the referenced-blob count;
/// runs on a slower cadence than the incremental reconcile.
async fn repair_blobs(
    client: &ClusterClient,
    metadata: &dyn MetadataStore,
    raw: &dyn RawBlobOps,
    cluster: &ClusterState,
) {
    let referenced = match metadata.list_referenced_blob_ids().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "blob repair: listing referenced blobs failed");
            return;
        }
    };
    // Repair only from ELIGIBLE peers (H12): bytes fetched from an
    // unauthenticated endpoint could be fabricated, and a wrong-master-key
    // peer's bytes would be undecryptable under our key.
    let peers: Vec<String> = cluster
        .peers()
        .into_iter()
        .filter(|p| p.eligible())
        .map(|p| p.endpoint)
        .collect();
    if peers.is_empty() {
        return;
    }

    let mut repaired = 0u64;
    for blob_id in &referenced {
        // Present locally → nothing to do. On a stat error, skip (conservative:
        // never attempt a repair we cannot first confirm is missing).
        if raw.exists(blob_id).await.unwrap_or(true) {
            continue;
        }
        match raw.read_sidecar(blob_id).await {
            // Composite blob: it has no file of its own; repair any missing parts.
            Ok(Some(meta)) if meta.composite.is_some() => {
                for part in meta.composite.unwrap() {
                    if !raw.exists(&part.blob_id).await.unwrap_or(true)
                        && fetch_and_store(client, raw, &peers, &part.blob_id).await
                    {
                        repaired += 1;
                    }
                }
            }
            // Normal blob (or sidecar also missing) → fetch it from a peer.
            _ => {
                if fetch_and_store(client, raw, &peers, blob_id).await {
                    repaired += 1;
                }
            }
        }
    }
    if repaired > 0 {
        tracing::info!(repaired, "blob repair: fetched missing blobs from peers");
    }
}

/// Fetches one blob (raw bytes + sidecar) from the first live peer that has it
/// and stores it verbatim. Composite blobs carry only the sidecar. Returns
/// whether a peer supplied it.
async fn fetch_and_store(
    client: &ClusterClient,
    raw: &dyn RawBlobOps,
    peers: &[String],
    blob_id: &BlobId,
) -> bool {
    for endpoint in peers {
        match client.fetch_blob(endpoint, blob_id).await {
            Ok((sidecar, stream)) => {
                if sidecar.composite.is_none() {
                    if let Err(e) = raw.write_raw(blob_id, stream).await {
                        tracing::warn!(error = %e, blob_id = %blob_id.0, "blob repair: write_raw failed");
                        continue;
                    }
                }
                if let Err(e) = raw.write_sidecar(blob_id, &sidecar).await {
                    tracing::warn!(error = %e, blob_id = %blob_id.0, "blob repair: write_sidecar failed");
                    continue;
                }
                return true;
            }
            Err(_) => continue,
        }
    }
    false
}

/// Reclaims orphan blob files: those on disk that no live object row, in-progress
/// part, or non-orphan composite sidecar references, AND that are older than the
/// grace window. Composite-aware (data-loss guard): a composite's part blobs are
/// kept alive only while the composite blob is still metadata-referenced.
///
/// Fail-safe: if ANY enumeration (referenced ids, sidecar list, a sidecar read,
/// the on-disk list) fails, the whole pass is skipped — GC never deletes on a
/// partially-computed referenced set.
async fn gc_blobs(metadata: &dyn MetadataStore, raw: &dyn RawBlobOps, grace: Duration) {
    // 1) Blobs referenced by metadata (live object rows + in-progress parts).
    let mut referenced: HashSet<BlobId> = match metadata.list_referenced_blob_ids().await {
        Ok(v) => v.into_iter().collect(),
        Err(e) => {
            tracing::debug!(error = %e, "blob GC: listing referenced blobs failed; skipping");
            return;
        }
    };

    // 2) Add the part blobs of every NON-orphan composite sidecar (a composite is
    // non-orphan iff its own blob is still metadata-referenced). Parts of an
    // orphaned composite (its object row gone) are intentionally left reclaimable.
    let sidecar_ids = match raw.list_sidecar_ids().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "blob GC: listing sidecars failed; skipping");
            return;
        }
    };
    for sid in &sidecar_ids {
        match raw.read_sidecar(sid).await {
            Ok(Some(meta)) => {
                if let Some(parts) = meta.composite {
                    if referenced.contains(sid) {
                        for p in parts {
                            referenced.insert(p.blob_id);
                        }
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                // A sidecar we cannot read might keep parts alive → do not risk it.
                tracing::debug!(error = %e, "blob GC: sidecar read failed; skipping");
                return;
            }
        }
    }

    // 3) On-disk blob files → reclaim the unreferenced, grace-expired ones.
    let on_disk = match raw.list_blob_ids().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "blob GC: listing on-disk blobs failed; skipping");
            return;
        }
    };
    let deletable = plan_blob_gc(&on_disk, &referenced, SystemTime::now(), grace);
    let mut reclaimed = 0u64;
    for id in &deletable {
        match raw.delete_blob_file(id).await {
            Ok(()) => reclaimed += 1,
            Err(e) => tracing::warn!(error = %e, blob_id = %id.0, "blob GC: delete failed"),
        }
    }
    if reclaimed > 0 {
        tracing::info!(reclaimed, "blob GC: reclaimed orphan blobs");
    }
}

/// Reconciles this node's control plane with a peer: pull the peer's snapshot,
/// compute the last-writer-wins merge against our own, and apply it. Identity
/// entities + tombstones go through the control-snapshot store; buckets go
/// through the metadata store so the metadata cache stays coherent.
///
// Covers EVERY control-plane family (R5 closed TD-016): the 5 original
// tombstoned families, the grant attachments/memberships, bucket_config,
// bucket_tags, cluster-wide server_config, and the in-progress multipart
// uploads with their parts (D4).
async fn reconcile_peer_control(
    client: &ClusterClient,
    control_snapshot: &dyn ControlSnapshotStore,
    metadata: &dyn MetadataStore,
    endpoint: &str,
) -> Result<(), ClusterError> {
    let remote = client.fetch_control_snapshot(endpoint).await?;
    let local = control_snapshot
        .build_control_snapshot()
        .await
        .map_err(|e| ClusterError::Serde(format!("build local snapshot: {e}")))?;
    let plan = plan_control_merge(&local, &remote);
    if plan.is_empty() {
        return Ok(());
    }
    // Identity entities (credentials/users/teams/grants, their attachments and
    // memberships, server_config) + tombstones. This runs BEFORE the
    // metadata-owned deletes below: it adopts ALL tombstones (bucket and
    // multipart ones included) first, so a crash between the two leaves the
    // safe state — tombstone present, row still alive — which converges on the
    // next round instead of resurrecting the deleted entity (review §2.3).
    control_snapshot
        .apply_control_merge(&plan)
        .await
        .map_err(|e| ClusterError::Serde(format!("apply control merge: {e}")))?;
    // Buckets — and their children, bucket config/tags — plus the multipart
    // rows go via the (cache-aware) metadata store. Order: bucket upserts
    // before their children's, deletes last (delete_bucket cascades config and
    // tags locally; delete_multipart_upload cascades part rows).
    for bucket in &plan.upsert_buckets {
        if let Err(e) = metadata.apply_remote_bucket(bucket).await {
            tracing::warn!(bucket = %bucket.name, error = %e, "anti-entropy: bucket upsert failed");
        }
    }
    for x in &plan.upsert_bucket_configs {
        if let Err(e) = metadata
            .apply_bucket_config_at(&x.bucket, &x.key, &x.value, x.updated_at)
            .await
        {
            tracing::warn!(bucket = %x.bucket, key = %x.key, error = %e, "anti-entropy: bucket_config upsert failed");
        }
    }
    for x in &plan.upsert_bucket_tags {
        if let Err(e) = metadata
            .apply_bucket_tags_at(&x.bucket, &x.tags, x.updated_at)
            .await
        {
            tracing::warn!(bucket = %x.bucket, error = %e, "anti-entropy: bucket_tags upsert failed");
        }
    }
    for record in &plan.upsert_multipart_uploads {
        if let Err(e) = metadata.apply_remote_multipart_upload(record).await {
            tracing::warn!(upload_id = %record.upload_id, error = %e, "anti-entropy: multipart upsert failed");
        }
    }
    for part in &plan.upsert_parts {
        if let Err(e) = metadata.put_part(part).await {
            tracing::warn!(upload_id = %part.upload_id, part = part.part_number, error = %e, "anti-entropy: part upsert failed");
        }
    }
    for (bucket, key) in &plan.delete_bucket_configs {
        if let Err(e) = metadata.delete_bucket_config(bucket, key).await {
            tracing::warn!(bucket = %bucket, key = %key, error = %e, "anti-entropy: bucket_config delete failed");
        }
    }
    for bucket in &plan.delete_bucket_tags {
        if let Err(e) = metadata.delete_bucket_tags(bucket).await {
            tracing::warn!(bucket = %bucket, error = %e, "anti-entropy: bucket_tags delete failed");
        }
    }
    for upload_id in &plan.delete_multipart_uploads {
        if let Err(e) = metadata.delete_multipart_upload(upload_id).await {
            tracing::warn!(upload_id = %upload_id, error = %e, "anti-entropy: multipart delete failed");
        }
    }
    for name in &plan.delete_buckets {
        if let Err(e) = metadata.delete_bucket(name).await {
            tracing::warn!(bucket = %name, error = %e, "anti-entropy: bucket delete failed");
        }
    }
    Ok(())
}

/// Pulls a peer's manifest from `since`, applying each batch in `seq` order,
/// until a short batch (caught up) or no progress (a failing entry, retried
/// next tick). Returns the high-water mark reached.
async fn reconcile_peer_objects(
    client: &ClusterClient,
    metadata: &dyn MetadataStore,
    endpoint: &str,
    since: u64,
) -> Result<u64, ClusterError> {
    let mut cursor = since;
    loop {
        let manifest = client.fetch_manifest(endpoint, cursor, MANIFEST_BATCH).await?;
        let batch_len = manifest.entries.len();
        let new_cursor = apply_entries(metadata, &manifest.entries, cursor).await;
        let progressed = new_cursor > cursor;
        cursor = new_cursor;
        if batch_len < MANIFEST_BATCH as usize || !progressed {
            break;
        }
    }
    Ok(cursor)
}

/// Applies manifest entries in ascending `seq` order, stopping at the first
/// failure so the unapplied tail is retried on the next pass. Returns the
/// highest `seq` successfully applied (or `floor` if none applied).
async fn apply_entries(metadata: &dyn MetadataStore, entries: &[ManifestEntry], floor: u64) -> u64 {
    let mut cursor = floor;
    for entry in entries {
        match metadata.apply_remote_object(&entry.record).await {
            Ok(()) => cursor = entry.seq,
            Err(e) => {
                tracing::warn!(
                    seq = entry.seq,
                    error = %e,
                    "anti-entropy: apply_remote_object failed; will retry"
                );
                break;
            }
        }
    }
    cursor
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::types::{BlobId, ObjectRecord};
    use chrono::TimeZone;

    async fn store() -> Arc<dyn MetadataStore> {
        Arc::new(arca_storage::SqliteStore::open_in_memory().await.unwrap())
    }

    fn rec(key: &str, last_modified_secs: i64) -> ObjectRecord {
        ObjectRecord {
            bucket: "b".to_string(),
            key: key.to_string(),
            blob_id: BlobId(format!("blob-{key}")),
            size: 4,
            etag: "e".to_string(),
            content_type: None,
            last_modified: Utc.timestamp_opt(last_modified_secs, 0).unwrap(),
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

    #[tokio::test]
    async fn apply_entries_applies_in_order_and_returns_cursor() {
        let m = store().await;
        m.create_bucket("b").await.unwrap();
        let entries = vec![
            ManifestEntry { seq: 5, record: rec("k1", 1000) },
            ManifestEntry { seq: 9, record: rec("k2", 1000) },
        ];
        let cursor = apply_entries(m.as_ref(), &entries, 0).await;
        assert_eq!(cursor, 9, "cursor advances to the last applied seq");
        assert!(m.get_object("b", "k1").await.unwrap().is_some());
        assert!(m.get_object("b", "k2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn apply_entries_empty_returns_floor() {
        let m = store().await;
        assert_eq!(apply_entries(m.as_ref(), &[], 7).await, 7);
    }

    #[tokio::test]
    async fn apply_entries_converges_tombstone_to_deletion() {
        // A manifest carrying a live row followed by a (newer) tombstone for the
        // same key must converge to "deleted" — the anti-entropy delete path.
        let m = store().await;
        m.create_bucket("b").await.unwrap();
        let live = rec("k", 1000);
        let mut tomb = rec("k", 2000); // newer than the live row
        tomb.is_tombstone = true;
        tomb.blob_id = BlobId(String::new());
        let entries = vec![
            ManifestEntry { seq: 1, record: live },
            ManifestEntry { seq: 2, record: tomb },
        ];
        apply_entries(m.as_ref(), &entries, 0).await;
        assert!(
            m.get_object("b", "k").await.unwrap().is_none(),
            "the tombstone must win and the object read as deleted"
        );
    }

    // --- blob GC (composite data-loss guard) --------------------------------

    fn one_chunk(data: &[u8]) -> arca_core::store::ByteStream {
        let b = bytes::Bytes::copy_from_slice(data);
        Box::pin(futures_util::stream::once(
            async move { Ok::<_, std::io::Error>(b) },
        ))
    }

    fn plain_sidecar() -> arca_core::store::SidecarMeta {
        arca_core::store::SidecarMeta {
            bucket: "b".to_string(),
            key: "k".to_string(),
            size: 1,
            etag: "e".to_string(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            metadata: Default::default(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    fn obj_with_blob(key: &str, blob_id: &str) -> ObjectRecord {
        let mut r = rec(key, 1000);
        r.blob_id = BlobId(blob_id.to_string());
        r
    }

    #[tokio::test]
    async fn gc_reclaims_orphans_and_protects_composite_parts() {
        use arca_core::store::{CompositePart, RawBlobOps, SidecarMeta};

        let dir = tempfile::tempdir().unwrap();
        let fs = arca_storage::FsBlobStore::new(dir.path().join("blobs"), 2)
            .await
            .unwrap();
        let m = store().await;
        m.create_bucket("b").await.unwrap();

        // Referenced normal blob.
        fs.write_raw(&BlobId("ref1".into()), one_chunk(b"x")).await.unwrap();
        RawBlobOps::write_sidecar(&fs, &BlobId("ref1".into()), &plain_sidecar()).await.unwrap();
        m.put_object(&obj_with_blob("k1", "ref1")).await.unwrap();

        // Orphan normal blob (no object row).
        fs.write_raw(&BlobId("orphan1".into()), one_chunk(b"y")).await.unwrap();
        RawBlobOps::write_sidecar(&fs, &BlobId("orphan1".into()), &plain_sidecar()).await.unwrap();

        // Live composite: part p1 kept alive by comp1 (which has an object row).
        fs.write_raw(&BlobId("p1".into()), one_chunk(b"z")).await.unwrap();
        let comp1 = SidecarMeta {
            composite: Some(vec![CompositePart {
                blob_id: BlobId("p1".into()),
                plaintext_size: 1,
                plaintext_etag: "e".into(),
                encryption: None,
            }]),
            ..plain_sidecar()
        };
        RawBlobOps::write_sidecar(&fs, &BlobId("comp1".into()), &comp1).await.unwrap();
        m.put_object(&obj_with_blob("k2", "comp1")).await.unwrap();

        // Orphan composite: part p2 kept alive ONLY by comp2, which has no row.
        fs.write_raw(&BlobId("p2".into()), one_chunk(b"w")).await.unwrap();
        let comp2 = SidecarMeta {
            composite: Some(vec![CompositePart {
                blob_id: BlobId("p2".into()),
                plaintext_size: 1,
                plaintext_etag: "e".into(),
                encryption: None,
            }]),
            ..plain_sidecar()
        };
        RawBlobOps::write_sidecar(&fs, &BlobId("comp2".into()), &comp2).await.unwrap();

        // grace = 0 so freshly-written blobs are immediately eligible.
        gc_blobs(m.as_ref(), &fs, Duration::ZERO).await;

        assert!(fs.exists(&BlobId("ref1".into())).await.unwrap(), "referenced blob kept");
        assert!(!fs.exists(&BlobId("orphan1".into())).await.unwrap(), "orphan blob reclaimed");
        assert!(fs.exists(&BlobId("p1".into())).await.unwrap(), "live composite part kept");
        assert!(
            !fs.exists(&BlobId("p2".into())).await.unwrap(),
            "orphan composite's part reclaimed"
        );
    }

    #[tokio::test]
    async fn gc_grace_protects_young_orphans() {
        use arca_core::store::RawBlobOps;
        let dir = tempfile::tempdir().unwrap();
        let fs = arca_storage::FsBlobStore::new(dir.path().join("blobs"), 2)
            .await
            .unwrap();
        let m = store().await;

        fs.write_raw(&BlobId("young".into()), one_chunk(b"x")).await.unwrap();
        RawBlobOps::write_sidecar(&fs, &BlobId("young".into()), &plain_sidecar()).await.unwrap();

        // Large grace: the just-written orphan is too young to reclaim.
        gc_blobs(m.as_ref(), &fs, Duration::from_secs(3600)).await;
        assert!(fs.exists(&BlobId("young".into())).await.unwrap());
    }
}
