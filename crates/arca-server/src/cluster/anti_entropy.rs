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
//! - **Tombstone GC**: drop tombstones older than the configured grace window
//!   (which must exceed the longest expected node downtime).
//!
//! The changed-since manifest is incremental and indexed by `seq`, so this can
//! run frequently and cheaply — that is why Arca relies on frequent anti-entropy
//! plus read-repair instead of the hinted-handoff machinery large clusters need
//! (their reconciliation is expensive and runs rarely).
//!
//! Control-plane reconciliation and blob GC/repair are layered onto this same
//! worker in following chunks.
//!
//! The high-water mark is per-peer and **in-memory**: this node's view of how
//! far it has consumed each peer's `seq`. It is node-local and must never be
//! replicated (it is meaningless elsewhere). On restart it resets to 0, costing
//! one extra full manifest pass per peer — idempotent, then incremental.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arca_core::cluster::{ClusterState, ManifestEntry};
use arca_core::store::MetadataStore;
use chrono::Utc;

use crate::cluster::client::{ClusterClient, ClusterError};
use crate::worker::BackgroundWorker;

/// Object manifest page size per request (the server clamps to its own max).
const MANIFEST_BATCH: u32 = 500;

/// Spawns the anti-entropy worker. It runs for the lifetime of the returned
/// handle, which the caller keeps alive.
pub fn spawn(
    cluster: Arc<ClusterState>,
    client: ClusterClient,
    metadata: Arc<dyn MetadataStore>,
    interval: Duration,
    tombstone_grace: Duration,
) -> BackgroundWorker {
    let handle = tokio::spawn(async move {
        // Per-peer high-water mark: highest seq applied from each peer node.
        let mut hwm: HashMap<String, u64> = HashMap::new();
        let mut timer = tokio::time::interval(interval.max(Duration::from_secs(1)));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        timer.tick().await; // skip the immediate first tick

        loop {
            timer.tick().await;

            // 1) Objects: pull each live peer's changed-since manifest.
            for peer in cluster.peers().into_iter().filter(|p| p.alive) {
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
            }

            // 2) Tombstone GC: drop tombstones past the grace window.
            let grace = chrono::Duration::from_std(tombstone_grace)
                .unwrap_or_else(|_| chrono::Duration::days(7));
            match metadata.purge_tombstones(Utc::now() - grace).await {
                Ok(n) if n > 0 => tracing::debug!(purged = n, "anti-entropy: tombstone GC"),
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "anti-entropy: tombstone GC failed"),
            }
        }
    });
    BackgroundWorker::from_handle(handle)
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
