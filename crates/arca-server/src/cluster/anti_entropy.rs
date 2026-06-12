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
//! The high-water mark is per-peer and **in-memory** (now carried by
//! [`ClusterState`] as part of the per-peer [`PeerSyncStatus`], so the
//! health/admin endpoints can expose it — review D2): this node's view of how
//! far it has consumed each peer's `seq`. It is node-local and must never be
//! replicated (it is meaningless elsewhere). On restart it resets to 0, costing
//! one extra full manifest pass per peer — idempotent, then incremental.
//!
//! R7 operability additions:
//! - **Syncing readiness (D2)**: the completion of the first full pass toward
//!   each peer is recorded in [`ClusterState`]; until every eligible peer has
//!   one, `/admin/health` reports `syncing` (503) and the LB keeps this node
//!   out of rotation, so a re-entering node serves no stale 404s/listings.
//! - **Rewind detection (D3c)**: a peer restored from backup reports (via the
//!   authenticated ping) a `max_seq` below our HWM — detected per-tick
//!   ([`arca_core::cluster::sync_rewound`]) and answered by resetting the HWM
//!   to 0 (one idempotent full re-pull).
//! - **Stuck-HWM skip (M1)**: a manifest entry that persistently fails to
//!   apply is skipped after [`STUCK_SKIP_AFTER`] consecutive passes (warn +
//!   `skipped_entries` evidence in `/admin/cluster`) instead of blocking that
//!   peer's incremental sync forever.
//! - **Repair budget (M2)**: the proactive blob-repair sweep attempts at most
//!   `[cluster] blob_repair_budget` peer fetches per tick, resuming where it
//!   left off on the next tick, so a huge backlog cannot monopolize the worker.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use arca_core::cluster::{
    plan_blob_gc, plan_control_merge, sync_rewound, tombstone_gc_blockers, ClusterState,
    ManifestEntry,
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

/// M1 — skip a manifest entry after this many CONSECUTIVE passes failed to
/// apply the same `seq`. Below the threshold a failure is treated as transient
/// (the safe default: the unapplied tail is simply retried next tick); past it
/// the entry is blocking that peer's whole incremental sync — a liveness
/// problem worse than the one skipped row, which converges anyway the next
/// time the key changes on the peer (and is counted as operator evidence).
const STUCK_SKIP_AFTER: u32 = 5;

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
    blob_repair_budget: u32,
) -> BackgroundWorker {
    let handle = tokio::spawn(async move {
        let mut timer = tokio::time::interval(interval.max(Duration::from_secs(1)));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        timer.tick().await; // skip the immediate first tick
        let mut tick: u64 = 0;
        // M1: per-peer (failing seq, consecutive-failure count).
        let mut stuck = StuckTracker::default();
        // M2: where the budget-bounded blob-repair sweep resumes mid-flight.
        let mut repair_cursor: Option<BlobId> = None;

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
                // 0) D3c — restore/rewind detection: the peer's ping-reported
                // seq counter fell below what we already consumed (it was
                // restored from a backup). Reset the HWM: its post-restore
                // writes re-use seq values below the old HWM and would
                // otherwise stay invisible here until OUR next restart.
                let sync = cluster.peer_sync(&peer.node_id);
                if sync_rewound(sync.hwm, sync.hwm_at, peer.max_seq, peer.last_seen) {
                    tracing::warn!(
                        peer = %peer.endpoint,
                        peer_node_id = %peer.node_id,
                        hwm = sync.hwm,
                        peer_max_seq = peer.max_seq.unwrap_or(0),
                        "anti-entropy: peer's object-seq counter REWOUND below our \
                         high-water mark (restored from backup?) — resetting the HWM \
                         and re-pulling its full manifest (idempotent)"
                    );
                    cluster.reset_sync_hwm(&peer.node_id);
                }

                // 1) Objects: pull this peer's changed-since manifest.
                let since = cluster.peer_sync(&peer.node_id).hwm;
                let mut objects_caught_up = false;
                match reconcile_peer_objects(&client, metadata.as_ref(), &peer.endpoint, since)
                    .await
                {
                    Ok(outcome) => {
                        let mut cursor = outcome.cursor;
                        // M1: a pass that keeps dying on the SAME entry is
                        // skipped past after STUCK_SKIP_AFTER attempts.
                        if stuck.observe(&peer.node_id, outcome.failed_seq) {
                            let seq = outcome.failed_seq.unwrap_or(cursor);
                            tracing::warn!(
                                peer = %peer.endpoint,
                                peer_node_id = %peer.node_id,
                                seq,
                                attempts = STUCK_SKIP_AFTER,
                                "anti-entropy: SKIPPING a manifest entry that persistently \
                                 fails to apply — that key may not converge on this node \
                                 until it changes again on the peer (counted as \
                                 skipped_entries in /admin/cluster)"
                            );
                            cluster.record_skipped_entry(&peer.node_id);
                            cursor = seq;
                        }
                        if cursor > since {
                            cluster.set_sync_hwm(&peer.node_id, cursor, Utc::now());
                        }
                        objects_caught_up = outcome.caught_up;
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
                match reconcile_peer_control(
                    &client,
                    control_snapshot.as_ref(),
                    metadata.as_ref(),
                    &peer.endpoint,
                )
                .await
                {
                    Ok(()) => {
                        // D2: a FULL pass (objects caught up + control merged)
                        // completed — stamp it; the first one per peer flips
                        // this node's readiness toward that peer.
                        if objects_caught_up {
                            cluster.record_reconcile_complete(&peer.node_id, Utc::now());
                        }
                    }
                    Err(e) => {
                        tracing::debug!(
                            peer = %peer.endpoint,
                            error = %e,
                            "anti-entropy: control reconcile failed (retried next tick)"
                        );
                    }
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
            // M2: the repair sweep is budget-bounded; while one is mid-flight
            // (cursor set) it continues on EVERY tick — only complete sweeps
            // wait for the slower cadence — so a big backlog drains at
            // `budget / interval` without monopolizing any single tick.
            if tick % BLOB_SCAN_EVERY_TICKS == 0 || repair_cursor.is_some() {
                repair_cursor = repair_blobs(
                    &client,
                    metadata.as_ref(),
                    raw.as_ref(),
                    &cluster,
                    repair_cursor.take(),
                    blob_repair_budget,
                )
                .await;
            }
            if tick % BLOB_SCAN_EVERY_TICKS == 0 {
                // Reuse the tombstone grace: like a tombstone, an orphan blob
                // must outlive the max reconcile lag before it is safe to reclaim.
                gc_blobs(metadata.as_ref(), raw.as_ref(), tombstone_grace).await;
            }
        }
    });
    BackgroundWorker::from_handle(handle)
}

/// M1 — per-peer stuck-entry bookkeeping: counts consecutive reconcile passes
/// that failed at the same manifest `seq`. [`StuckTracker::observe`] returns
/// `true` when the entry has hit [`STUCK_SKIP_AFTER`] and should be skipped
/// NOW (the streak resets — a later failure on the same seq starts over).
#[derive(Default)]
struct StuckTracker(HashMap<String, (u64, u32)>);

impl StuckTracker {
    fn observe(&mut self, node_id: &str, failed_seq: Option<u64>) -> bool {
        let Some(seq) = failed_seq else {
            // A clean pass (or a different failure mode): no streak to keep.
            self.0.remove(node_id);
            return false;
        };
        let entry = self.0.entry(node_id.to_string()).or_insert((seq, 0));
        if entry.0 != seq {
            // Progress was made and a DIFFERENT entry now fails: new streak.
            *entry = (seq, 1);
            return false;
        }
        entry.1 += 1;
        if entry.1 >= STUCK_SKIP_AFTER {
            self.0.remove(node_id);
            return true;
        }
        false
    }
}

/// Proactively repairs locally-missing blob bytes: for every blob_id referenced
/// by metadata, if the physical file is absent, fetch it (or, for a composite,
/// its missing parts) from a live peer.
///
/// M2 — budget-bounded: at most `budget` peer fetches are attempted per call
/// (the local existence checks are cheap stats and are not budgeted; network
/// fetches are what monopolize the worker). The sweep iterates the referenced
/// ids in sorted order so `resume` (the last fully-processed id of the
/// previous call) makes it restartable: the return value is `Some(cursor)`
/// when the budget ran out mid-sweep — pass it back next tick — or `None`
/// when the sweep completed. The budget is only checked BETWEEN blob ids: one
/// composite is always processed to completion (bounded overshoot), so a
/// composite with more permanently-unfetchable parts than the whole budget
/// cannot stall the sweep's progress forever.
async fn repair_blobs(
    client: &ClusterClient,
    metadata: &dyn MetadataStore,
    raw: &dyn RawBlobOps,
    cluster: &ClusterState,
    resume: Option<BlobId>,
    budget: u32,
) -> Option<BlobId> {
    let mut referenced = match metadata.list_referenced_blob_ids().await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "blob repair: listing referenced blobs failed");
            return None;
        }
    };
    referenced.sort_unstable_by(|a, b| a.0.cmp(&b.0));
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
        return None;
    }

    let mut repaired = 0u64;
    let mut attempts = 0u32;
    let mut cursor = resume;
    for blob_id in &referenced {
        if let Some(ref c) = cursor {
            if blob_id.0 <= c.0 {
                continue; // already processed by the previous call(s)
            }
        }
        if attempts >= budget {
            tracing::debug!(
                attempts,
                repaired,
                "blob repair: per-tick budget exhausted; sweep resumes next tick"
            );
            if repaired > 0 {
                tracing::info!(repaired, "blob repair: fetched missing blobs from peers");
            }
            return cursor;
        }
        // Present locally → nothing to do. On a stat error, skip (conservative:
        // never attempt a repair we cannot first confirm is missing).
        if raw.exists(blob_id).await.unwrap_or(true) {
            cursor = Some(blob_id.clone());
            continue;
        }
        match raw.read_sidecar(blob_id).await {
            // Composite blob: it has no file of its own; repair any missing parts.
            Ok(Some(meta)) if meta.composite.is_some() => {
                for part in meta.composite.unwrap() {
                    if !raw.exists(&part.blob_id).await.unwrap_or(true) {
                        attempts += 1;
                        if fetch_and_store(client, raw, &peers, &part.blob_id).await {
                            repaired += 1;
                        }
                    }
                }
            }
            // Normal blob (or sidecar also missing) → fetch it from a peer.
            _ => {
                attempts += 1;
                if fetch_and_store(client, raw, &peers, blob_id).await {
                    repaired += 1;
                }
            }
        }
        cursor = Some(blob_id.clone());
    }
    if repaired > 0 {
        tracing::info!(repaired, "blob repair: fetched missing blobs from peers");
    }
    None
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

/// What one object-reconcile pass toward a peer achieved.
struct ObjectsReconcileOutcome {
    /// The high-water mark reached (highest `seq` successfully applied).
    cursor: u64,
    /// Whether the peer's manifest was drained to the end with every entry
    /// applied — the "objects half" of a completed full pass (D2).
    caught_up: bool,
    /// The `seq` of the entry whose apply failed, when one did (feeds the M1
    /// stuck-entry tracker).
    failed_seq: Option<u64>,
}

/// Pulls a peer's manifest from `since`, applying each batch in `seq` order,
/// until a short batch (caught up) or no progress (a failing entry, retried
/// next tick).
async fn reconcile_peer_objects(
    client: &ClusterClient,
    metadata: &dyn MetadataStore,
    endpoint: &str,
    since: u64,
) -> Result<ObjectsReconcileOutcome, ClusterError> {
    let mut cursor = since;
    loop {
        let manifest = client.fetch_manifest(endpoint, cursor, MANIFEST_BATCH).await?;
        let batch_len = manifest.entries.len();
        let (new_cursor, failed_seq) = apply_entries(metadata, &manifest.entries, cursor).await;
        let progressed = new_cursor > cursor;
        cursor = new_cursor;
        if failed_seq.is_some() || !progressed {
            return Ok(ObjectsReconcileOutcome {
                cursor,
                caught_up: failed_seq.is_none() && batch_len < MANIFEST_BATCH as usize,
                failed_seq,
            });
        }
        if batch_len < MANIFEST_BATCH as usize {
            return Ok(ObjectsReconcileOutcome { cursor, caught_up: true, failed_seq: None });
        }
    }
}

/// Applies manifest entries in ascending `seq` order, stopping at the first
/// failure so the unapplied tail is retried on the next pass. Returns the
/// highest `seq` successfully applied (or `floor` if none applied) and the
/// failing entry's `seq`, if any (M1 evidence).
async fn apply_entries(
    metadata: &dyn MetadataStore,
    entries: &[ManifestEntry],
    floor: u64,
) -> (u64, Option<u64>) {
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
                return (cursor, Some(entry.seq));
            }
        }
    }
    (cursor, None)
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
            lock_updated_at: None,
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
        let (cursor, failed) = apply_entries(m.as_ref(), &entries, 0).await;
        assert_eq!(cursor, 9, "cursor advances to the last applied seq");
        assert_eq!(failed, None);
        assert!(m.get_object("b", "k1").await.unwrap().is_some());
        assert!(m.get_object("b", "k2").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn apply_entries_empty_returns_floor() {
        let m = store().await;
        assert_eq!(apply_entries(m.as_ref(), &[], 7).await, (7, None));
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

    // --- M1: stuck-entry skip decision ---------------------------------------

    #[test]
    fn stuck_tracker_skips_after_consecutive_failures_on_same_seq() {
        let mut t = StuckTracker::default();
        for attempt in 1..STUCK_SKIP_AFTER {
            assert!(
                !t.observe("n2", Some(42)),
                "attempt {attempt} below the threshold must not skip"
            );
        }
        assert!(t.observe("n2", Some(42)), "threshold reached: skip now");
        // The streak was consumed: a fresh failure on the same seq starts over.
        assert!(!t.observe("n2", Some(42)));
    }

    #[test]
    fn stuck_tracker_resets_on_progress_or_different_seq() {
        let mut t = StuckTracker::default();
        for _ in 0..STUCK_SKIP_AFTER - 1 {
            assert!(!t.observe("n2", Some(42)));
        }
        // A clean pass clears the streak entirely.
        assert!(!t.observe("n2", None));
        for _ in 0..STUCK_SKIP_AFTER - 1 {
            assert!(!t.observe("n2", Some(42)));
        }
        // A DIFFERENT failing seq means progress was made: new streak.
        assert!(!t.observe("n2", Some(99)));
        for _ in 0..STUCK_SKIP_AFTER - 2 {
            assert!(!t.observe("n2", Some(99)));
        }
        assert!(t.observe("n2", Some(99)));
    }

    #[test]
    fn stuck_tracker_tracks_peers_independently() {
        let mut t = StuckTracker::default();
        for _ in 0..STUCK_SKIP_AFTER - 1 {
            assert!(!t.observe("n2", Some(42)));
            assert!(!t.observe("n3", Some(42)));
        }
        assert!(t.observe("n2", Some(42)));
        assert!(t.observe("n3", Some(42)));
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

    // --- M2: budget-bounded, resumable blob-repair sweep ---------------------

    /// Minimal HTTP/1.1 peer answering every GET with `200 OK`, a plain
    /// sidecar in the cluster sidecar header and one byte of body — what
    /// `ClusterClient::fetch_blob` consumes for repair (same fake as the
    /// cluster_blob tests).
    async fn spawn_repair_peer() -> (String, tokio::task::JoinHandle<()>) {
        use base64::engine::general_purpose::STANDARD as BASE64;
        use base64::Engine;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let sidecar_b64 = BASE64.encode(serde_json::to_vec(&plain_sidecar()).unwrap());
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let sidecar_b64 = sidecar_b64.clone();
                tokio::spawn(async move {
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
                         {}: {sidecar_b64}\r\ncontent-length: 1\r\nconnection: close\r\n\r\nx",
                        arca_core::cluster::CLUSTER_SIDECAR_HEADER,
                    );
                    let _ = sock.write_all(head.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn eligible_peer_at(endpoint: &str) -> arca_core::cluster::PeerNode {
        arca_core::cluster::PeerNode {
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

    #[tokio::test]
    async fn repair_budget_bounds_fetches_and_cursor_resumes_the_sweep() {
        use crate::cluster::client::ClusterClient;

        let dir = tempfile::tempdir().unwrap();
        let fs = arca_storage::FsBlobStore::new(dir.path().join("blobs"), 2)
            .await
            .unwrap();
        let m = store().await;
        m.create_bucket("b").await.unwrap();
        // Four referenced blobs, none present locally → four repairs needed.
        for k in ["k1", "k2", "k3", "k4"] {
            m.put_object(&rec(k, 1000)).await.unwrap();
        }

        let (endpoint, _h) = spawn_repair_peer().await;
        let cluster = ClusterState::new("self", None, None);
        cluster.set_peers(vec![eligible_peer_at(&endpoint)]);
        let client =
            ClusterClient::new("self", "secret", Duration::from_secs(2), None).unwrap();

        // Budget 2: the first call repairs exactly two and returns a cursor.
        let cursor = repair_blobs(&client, m.as_ref(), &fs, &cluster, None, 2).await;
        assert!(cursor.is_some(), "budget exhausted mid-sweep → resumable cursor");
        let mut present = 0;
        for k in ["k1", "k2", "k3", "k4"] {
            if fs.exists(&BlobId(format!("blob-{k}"))).await.unwrap() {
                present += 1;
            }
        }
        assert_eq!(present, 2, "exactly the budgeted number of fetches");

        // Resuming completes the sweep (no re-fetch of the repaired ones:
        // the cursor skips them) and reports completion with None.
        let cursor = repair_blobs(&client, m.as_ref(), &fs, &cluster, cursor, 2).await;
        assert_eq!(cursor.map(|c| c.0), None, "sweep completed");
        for k in ["k1", "k2", "k3", "k4"] {
            assert!(fs.exists(&BlobId(format!("blob-{k}"))).await.unwrap(), "{k} repaired");
        }
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
