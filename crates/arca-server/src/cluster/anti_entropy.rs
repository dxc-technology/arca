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
//!   downtime).
//!
//! The changed-since manifest is incremental and indexed by `seq`, so this can
//! run frequently and cheaply — that is why Arca relies on frequent anti-entropy
//! plus read-repair instead of the hinted-handoff machinery large clusters need
//! (their reconciliation is expensive and runs rarely).
//!
//! Blob GC/repair is layered onto this same worker in a following chunk.
//!
//! The high-water mark is per-peer and **in-memory**: this node's view of how
//! far it has consumed each peer's `seq`. It is node-local and must never be
//! replicated (it is meaningless elsewhere). On restart it resets to 0, costing
//! one extra full manifest pass per peer — idempotent, then incremental.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arca_core::cluster::{plan_control_merge, ClusterState, ManifestEntry};
use arca_core::store::{ControlSnapshotStore, ControlTombstoneStore, MetadataStore, RawBlobOps};
use arca_core::types::BlobId;
use chrono::Utc;

use crate::cluster::client::{ClusterClient, ClusterError};
use crate::worker::BackgroundWorker;

/// Object manifest page size per request (the server clamps to its own max).
const MANIFEST_BATCH: u32 = 500;

/// Run the proactive blob repair scan every N anti-entropy ticks. It is an
/// O(referenced) stat sweep, so it runs on a slower cadence than the cheap
/// incremental object/control reconcile (lazy read-repair on GET still covers
/// on-access correctness between sweeps).
const BLOB_REPAIR_EVERY_TICKS: u64 = 10;

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

            for peer in cluster.peers().into_iter().filter(|p| p.alive) {
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

            // 3) Tombstone GC: drop object AND control tombstones past the grace.
            let grace = chrono::Duration::from_std(tombstone_grace)
                .unwrap_or_else(|_| chrono::Duration::days(7));
            let cutoff = Utc::now() - grace;
            match metadata.purge_tombstones(cutoff).await {
                Ok(n) if n > 0 => tracing::debug!(purged = n, "anti-entropy: object tombstone GC"),
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "anti-entropy: object tombstone GC failed"),
            }
            match control_tombstone.purge_control_tombstones(cutoff).await {
                Ok(n) if n > 0 => {
                    tracing::debug!(purged = n, "anti-entropy: control tombstone GC")
                }
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "anti-entropy: control tombstone GC failed"),
            }

            // 4) Blob repair (slower cadence): proactively fetch bytes for rows
            // whose blob is missing locally, so durability does not wait for a
            // GET to trigger the lazy read-repair.
            if tick % BLOB_REPAIR_EVERY_TICKS == 0 {
                repair_blobs(&client, metadata.as_ref(), raw.as_ref(), &cluster).await;
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
    let peers: Vec<String> = cluster
        .peers()
        .into_iter()
        .filter(|p| p.alive)
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

/// Reconciles this node's control plane with a peer: pull the peer's snapshot,
/// compute the last-writer-wins merge against our own, and apply it. Identity
/// entities + tombstones go through the control-snapshot store; buckets go
/// through the metadata store so the metadata cache stays coherent.
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
    // Identity entities (credentials/users/teams/grants) + tombstones.
    control_snapshot
        .apply_control_merge(&plan)
        .await
        .map_err(|e| ClusterError::Serde(format!("apply control merge: {e}")))?;
    // Buckets via the (cache-aware) metadata store.
    for bucket in &plan.upsert_buckets {
        if let Err(e) = metadata.apply_remote_bucket(bucket).await {
            tracing::warn!(bucket = %bucket.name, error = %e, "anti-entropy: bucket upsert failed");
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
}
