//! Maintenance-jobs worker (Phase 30).
//!
//! A single background worker processes at most one maintenance job at a time,
//! committing progress per item so pause / cancel / restart are always safe. It
//! is leader-gated in a cluster (decision H5/R6): the job mutates fully
//! replicated data, so only the worker-leader node runs it.
//!
//! A job declaring `mode = maintenance` drains the S3 API on this node for its
//! whole active lifetime (via the Phase 23 drain channel, reused here): the
//! health endpoint reports `draining` so the load balancer stops routing S3
//! traffic. The admin API and the worker stay live throughout.

use std::sync::Arc;
use std::time::Duration;

use arca_core::store::maintenance::{
    MaintenanceJob, MaintenanceJobStatus, MaintenanceStore, DEFAULT_MAX_JOB_LOGS,
};
use arca_core::store::{BlobStore, MetadataStore, SidecarMeta};
use arca_core::types::{BlobId, ObjectRecord};
use arca_proto::state::AppState;
use arca_storage::encryption::keys::MasterKey;
use arca_storage::recrypt::{self, RecryptDirection};
use tokio::sync::watch;

use crate::config::StorageConfig;
use crate::worker::BackgroundWorker;

/// Dependencies the re-encryption job types (`encrypt` / `decrypt`) need beyond
/// the maintenance store: the metadata store (for the candidate scan + the CAS
/// row swap) and the blob stores (encrypting + plain) plus the master key.
/// `None` for deployments without encryption configured (encrypt/decrypt jobs
/// then fail cleanly with a clear message).
#[derive(Clone)]
pub struct RecryptCtx {
    pub metadata: Arc<dyn MetadataStore>,
    /// Encryption-aware blob store: `get` returns plaintext for any blob,
    /// `put` writes an encrypted blob.
    pub blob: Arc<dyn BlobStore>,
    /// Plain (non-encrypting) blob store, present when encryption is configured.
    pub plain_blob: Option<Arc<dyn BlobStore>>,
    /// Master key, present when encryption is configured.
    pub master_key: Option<Arc<MasterKey>>,
    /// Whether this node is part of a cluster. Governs old-blob reclaim after a
    /// copy-on-write swap: on a single node the old blob is deleted eagerly; in
    /// a cluster it is left for the grace-bounded anti-entropy GC instead, so
    /// old-blob reclaim never races the blob-repair pulls peers issue while they
    /// converge onto the re-encrypted row (see TECH_DEBT TD-021).
    pub clustered: bool,
}

/// Dependencies the `migrate-db` job needs: the storage config (to open the
/// TARGET backend) and the SOURCE backend name (the running `metadata_backend`).
/// The SOURCE store is opened fresh from the same config, so the job never
/// touches the live store handles.
#[derive(Clone)]
pub struct MigrateDbCtx {
    pub storage: StorageConfig,
}

/// How often the worker polls for an active job and advances it.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Spawns the maintenance-jobs worker. `drain_tx` is the maintenance-drain
/// channel whose receiver lives in [`AppState::maintenance_draining`].
pub fn spawn_maintenance_worker(
    state: &AppState,
    drain_tx: Arc<watch::Sender<bool>>,
    master_key: Option<Arc<MasterKey>>,
    storage: StorageConfig,
) -> BackgroundWorker {
    let store = state.maintenance_store.clone();
    let cluster = state.cluster.clone();
    let recrypt = RecryptCtx {
        metadata: state.metadata.clone(),
        blob: state.blob.clone(),
        plain_blob: state.plain_blob.clone(),
        master_key,
        clustered: cluster.is_some(),
    };
    let migrate = MigrateDbCtx { storage };

    BackgroundWorker::spawn_periodic("maintenance", TICK_INTERVAL, move || {
        let store = store.clone();
        let cluster = cluster.clone();
        let drain_tx = drain_tx.clone();
        let recrypt = recrypt.clone();
        let migrate = migrate.clone();
        async move {
            let Some(store) = store else { return };
            // In a cluster only the worker-leader runs jobs (the data is fully
            // replicated). A non-leader must not hold the drain.
            if let Some(ref c) = cluster {
                if !c.is_worker_leader() {
                    set_drain(&drain_tx, false);
                    return;
                }
            }
            maintenance_tick(store.as_ref(), Some(&recrypt), Some(&migrate), &drain_tx).await;
        }
    })
}

fn set_drain(drain_tx: &watch::Sender<bool>, want: bool) {
    if *drain_tx.borrow() != want {
        let _ = drain_tx.send(want);
    }
}

/// One worker tick: reconcile the drain state from the active job, then advance
/// the job. Pure enough to unit-test against an in-memory store.
pub async fn maintenance_tick(
    store: &dyn MaintenanceStore,
    recrypt: Option<&RecryptCtx>,
    migrate: Option<&MigrateDbCtx>,
    drain_tx: &watch::Sender<bool>,
) {
    let active = match store.active_job().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "maintenance: active_job query failed");
            return;
        }
    };

    // The S3 API is drained whenever a maintenance-mode job is active (running
    // OR paused), and released as soon as the slot is free again.
    let want_drain = active.as_ref().is_some_and(|j| j.mode == "maintenance");
    set_drain(drain_tx, want_drain);

    let Some(job) = active else { return };
    match MaintenanceJobStatus::parse(&job.status) {
        Some(MaintenanceJobStatus::Pending) => {
            if store
                .set_job_status(&job.id, MaintenanceJobStatus::Running, None)
                .await
                .unwrap_or(false)
            {
                let _ = store
                    .append_job_log(&job.id, "info", "job started", DEFAULT_MAX_JOB_LOGS)
                    .await;
                process_job(store, recrypt, migrate, &job).await;
            }
        }
        Some(MaintenanceJobStatus::Running) => process_job(store, recrypt, migrate, &job).await,
        // Paused: idle (drain stays engaged per the reconcile above). Terminal:
        // nothing to do (active_job never returns terminal jobs anyway).
        _ => {}
    }
}

/// Dispatches a job to its processor. New job types plug in here (M3: migrate-db;
/// M4: migrate-topology).
async fn process_job(
    store: &dyn MaintenanceStore,
    recrypt: Option<&RecryptCtx>,
    migrate: Option<&MigrateDbCtx>,
    job: &MaintenanceJob,
) {
    let result = match job.job_type.as_str() {
        "noop" => process_noop(store, job).await,
        "encrypt" => match recrypt {
            Some(ctx) => process_recrypt(store, ctx, job, RecryptDirection::Encrypt).await,
            None => Err("re-encryption context unavailable".to_string()),
        },
        "decrypt" => match recrypt {
            Some(ctx) => process_recrypt(store, ctx, job, RecryptDirection::Decrypt).await,
            None => Err("re-encryption context unavailable".to_string()),
        },
        "migrate-db" => match migrate {
            Some(ctx) => process_migrate_db(store, ctx, job).await,
            None => Err("metadata-migration context unavailable".to_string()),
        },
        other => Err(format!("unknown job type: {other}")),
    };
    if let Err(e) = result {
        let _ = store
            .set_job_status(&job.id, MaintenanceJobStatus::Failed, Some(&e))
            .await;
        let _ = store
            .append_job_log(&job.id, "error", &e, DEFAULT_MAX_JOB_LOGS)
            .await;
        tracing::warn!(job = %job.id, error = %e, "maintenance: job failed");
    }
}

/// The trivial job type used to exercise the subsystem (and the M1 tests):
/// counts from `done` to `total`, honoring pause/cancel between items. Params:
/// `{"n": <total>, "delay_ms": <optional per-item sleep>}`.
async fn process_noop(store: &dyn MaintenanceStore, job: &MaintenanceJob) -> Result<(), String> {
    let total = job
        .params
        .get("n")
        .and_then(|v| v.as_u64())
        .unwrap_or(job.total);
    let delay_ms = job.params.get("delay_ms").and_then(|v| v.as_u64()).unwrap_or(0);

    let mut done = job.done;
    while done < total {
        // Re-read the live status so a pause/cancel issued via the admin API
        // mid-run takes effect promptly.
        match store.get_job(&job.id).await {
            Ok(Some(j)) if j.status == "running" => {}
            Ok(Some(_)) => return Ok(()), // paused or cancelled: stop, keep progress
            _ => return Ok(()),
        }
        done += 1;
        store
            .update_job_progress(&job.id, done, total, 0.0)
            .await
            .map_err(|e| e.to_string())?;
        if delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
    }

    store
        .set_job_status(&job.id, MaintenanceJobStatus::Completed, None)
        .await
        .map_err(|e| e.to_string())?;
    let _ = store
        .append_job_log(&job.id, "info", "job completed", DEFAULT_MAX_JOB_LOGS)
        .await;
    Ok(())
}

/// The `migrate-db` job: copies ALL metadata from the running (SOURCE) backend
/// into the TARGET backend named in `params.target`, in place. Blob files are
/// not touched. This is maintenance-mode (the S3 API is drained); the operator
/// switches `metadata_backend` in the config and restarts onto the new backend
/// afterward. Params: `{ "target": "sqlite"|"postgres", "force": bool }`.
async fn process_migrate_db(
    store: &dyn MaintenanceStore,
    ctx: &MigrateDbCtx,
    job: &MaintenanceJob,
) -> Result<(), String> {
    use arca_storage::migration::{self, TABLES};

    let from = ctx.storage.metadata_backend.as_str();
    let to = job
        .params
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "migrate-db requires a \"target\" param (sqlite|postgres)".to_string())?;
    let force = job
        .params
        .get("force")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if to != "sqlite" && to != "postgres" {
        return Err(format!("invalid target backend \"{to}\" (expected sqlite|postgres)"));
    }
    if to == from {
        return Err(format!(
            "target backend \"{to}\" equals the source backend; nothing to migrate"
        ));
    }

    // Open SOURCE (the running backend) and TARGET fresh from config; never touch
    // the live store handles.
    let source = crate::migrate_db::open_backend_from_storage(&ctx.storage, from)
        .await
        .map_err(|e| format!("opening source backend ({from}): {e}"))?;
    let dest = crate::migrate_db::open_backend_from_storage(&ctx.storage, to)
        .await
        .map_err(|e| format!("opening target backend ({to}): {e}"))?;

    let total_tables = TABLES.len() as u64;
    store
        .update_job_progress(&job.id, 0, total_tables, 0.0)
        .await
        .map_err(|e| e.to_string())?;
    let _ = store
        .append_job_log(
            &job.id,
            "info",
            &format!("migrating metadata {from} -> {to} (force={force})"),
            DEFAULT_MAX_JOB_LOGS,
        )
        .await;

    // The progress callback is synchronous (migrate_all cannot await per table),
    // so it can only trace; the durable per-table accounting is the report's
    // per-table counts, logged below, and the job's done/total set after the
    // copy. Table count is small and the run is drained, so coarse progress
    // (0 -> total on completion) is acceptable.
    // TECHDEBT(TD-022): the copy does not poll the pause/cancel flag between
    // tables, so a cancel only takes effect once the whole copy finishes; on
    // error the destination is left partial and must be dropped before retry.
    let report = migration::migrate_all(&source, &dest, force, |idx, total, name| {
        tracing::info!(table = name, step = idx + 1, total, "migrate-db: copying table");
    })
    .await?;

    for t in &report.tables {
        let _ = store
            .append_job_log(
                &job.id,
                "info",
                &format!("{}: {} row(s)", t.table, t.source_count),
                DEFAULT_MAX_JOB_LOGS,
            )
            .await;
    }

    store
        .update_job_progress(&job.id, total_tables, total_tables, 0.0)
        .await
        .map_err(|e| e.to_string())?;
    store
        .set_job_status(&job.id, MaintenanceJobStatus::Completed, None)
        .await
        .map_err(|e| e.to_string())?;
    let _ = store
        .append_job_log(
            &job.id,
            "info",
            &format!(
                "migration completed: {} table(s), {} row(s) copied {from} -> {to}. \
                 Switch metadata_backend to \"{to}\" and restart.",
                report.tables.len(),
                report.total_rows
            ),
            DEFAULT_MAX_JOB_LOGS,
        )
        .await;
    Ok(())
}

/// Whether an etag is a multipart/composite etag (`<32 hex>-<n>`). Multipart
/// objects are composite blobs (TD-014) and are skipped by re-encryption jobs.
fn is_multipart_etag(etag: &str) -> bool {
    match etag.rsplit_once('-') {
        Some((hash, count)) => {
            hash.len() == 32
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && !count.is_empty()
                && count.bytes().all(|b| b.is_ascii_digit())
        }
        None => false,
    }
}

/// Whether an object row is a candidate for the given re-encryption direction.
/// Skips delete markers / tombstones / empty blobs, SSE-C (customer key),
/// multipart/composite (TD-014), and rows already in the target state.
fn is_recrypt_candidate(r: &ObjectRecord, direction: RecryptDirection) -> bool {
    if r.is_delete_marker || r.is_tombstone || r.blob_id.0.is_empty() {
        return false;
    }
    if r.encryption_algorithm.as_deref() == Some(recrypt::SSEC) {
        return false;
    }
    if is_multipart_etag(&r.etag) {
        return false;
    }
    match direction {
        RecryptDirection::Encrypt => r.encryption_algorithm.is_none(),
        RecryptDirection::Decrypt => r.encryption_algorithm.as_deref() == Some(recrypt::AES256),
    }
}

/// Scans every bucket's object versions and returns the rows that still need
/// the transform. In-memory for the MVP; large stores are noted as a limitation.
async fn scan_candidates(
    ctx: &RecryptCtx,
    direction: RecryptDirection,
    bucket_filter: Option<&str>,
    prefix: Option<&str>,
) -> Result<Vec<ObjectRecord>, String> {
    let buckets = ctx
        .metadata
        .list_buckets()
        .await
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for b in buckets {
        if let Some(bf) = bucket_filter {
            if b.name != bf {
                continue;
            }
        }
        let mut key_marker: Option<String> = None;
        let mut vid_marker: Option<String> = None;
        loop {
            let page = ctx
                .metadata
                .list_object_versions(
                    &b.name,
                    prefix,
                    key_marker.as_deref(),
                    vid_marker.as_deref(),
                    1000,
                )
                .await
                .map_err(|e| e.to_string())?;
            if page.is_empty() {
                break;
            }
            let last = page.last().unwrap();
            key_marker = Some(last.key.clone());
            vid_marker = last.version_id.clone();
            let page_len = page.len();
            for r in page {
                if is_recrypt_candidate(&r, direction) {
                    out.push(r);
                }
            }
            if page_len < 1000 {
                break;
            }
        }
    }
    Ok(out)
}

/// Copy-on-write re-encryption (encrypt or decrypt) of every candidate object.
/// Always COW (new blob_id) so it is safe with live readers AND respects the
/// cluster's blob_id immutability: the new blob propagates to peers via
/// read-repair / anti-entropy, the CAS row swap converges via lock_updated_at,
/// and the orphaned old blob is reclaimed by GC. `maintenance` mode runs at full
/// speed (S3 is drained); `live` mode honors the optional byte-rate throttle.
async fn process_recrypt(
    store: &dyn MaintenanceStore,
    ctx: &RecryptCtx,
    job: &MaintenanceJob,
    direction: RecryptDirection,
) -> Result<(), String> {
    // Both directions need encryption configured: encrypt needs the encrypting
    // blob store + master key, decrypt needs the plain store to write plaintext
    // and the master key to read the ciphertext.
    if ctx.master_key.is_none() || ctx.plain_blob.is_none() {
        return Err("encryption is not configured (no master key) — cannot run a re-encryption job".to_string());
    }

    let bucket_filter = job.params.get("bucket").and_then(|v| v.as_str());
    let prefix = job.params.get("prefix").and_then(|v| v.as_str());
    let rate_bps = job
        .params
        .get("rate_bytes_per_sec")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let live = job.mode == "live";

    let candidates = scan_candidates(ctx, direction, bucket_filter, prefix).await?;
    let total = candidates.len() as u64;
    store
        .update_job_progress(&job.id, 0, total, 0.0)
        .await
        .map_err(|e| e.to_string())?;
    let _ = store
        .append_job_log(
            &job.id,
            "info",
            &format!("re-encryption: {total} candidate object(s)"),
            DEFAULT_MAX_JOB_LOGS,
        )
        .await;

    let mut done = 0u64;
    let mut errors = 0u64;
    for record in candidates {
        // Honor pause/cancel issued via the admin API mid-run.
        match store.get_job(&job.id).await {
            Ok(Some(j)) if j.status == "running" => {}
            Ok(Some(_)) => return Ok(()),
            _ => return Ok(()),
        }
        match recrypt_one(ctx, &record, direction).await {
            Ok(_) => {}
            Err(e) => {
                errors += 1;
                let _ = store
                    .append_job_log(
                        &job.id,
                        "warn",
                        &format!("{}/{}: {e}", record.bucket, record.key),
                        DEFAULT_MAX_JOB_LOGS,
                    )
                    .await;
            }
        }
        done += 1;
        store
            .update_job_progress(&job.id, done, total, 0.0)
            .await
            .map_err(|e| e.to_string())?;

        // Byte-rate throttle (live mode only; maintenance mode runs drained).
        if live && rate_bps > 0 && record.size > 0 {
            let ms = record.size.saturating_mul(1000) / rate_bps;
            if ms > 0 {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
    }

    store
        .set_job_status(&job.id, MaintenanceJobStatus::Completed, None)
        .await
        .map_err(|e| e.to_string())?;
    let _ = store
        .append_job_log(
            &job.id,
            "info",
            &format!("re-encryption completed: {done} processed, {errors} error(s)"),
            DEFAULT_MAX_JOB_LOGS,
        )
        .await;
    Ok(())
}

/// Re-encrypts a single object via copy-on-write. Returns `Ok(true)` when the
/// CAS swap landed, `Ok(false)` when a concurrent client overwrite won the CAS
/// (the freshly written blob is discarded).
async fn recrypt_one(
    ctx: &RecryptCtx,
    record: &ObjectRecord,
    direction: RecryptDirection,
) -> Result<bool, String> {
    let old_blob_id = record.blob_id.clone();
    let new_blob_id = BlobId::new();
    let version_id = record.version_id.clone();

    // Plaintext source: the encrypting store returns plaintext for any blob
    // (passes through plain blobs, decrypts encrypted ones).
    let plaintext = ctx
        .blob
        .get(&old_blob_id, None)
        .await
        .map_err(|e| format!("read source blob: {e}"))?;

    let (put_result, write_store, encryption) = match direction {
        RecryptDirection::Encrypt => {
            let res = ctx
                .blob
                .put(&new_blob_id, plaintext.stream)
                .await
                .map_err(|e| format!("write encrypted blob: {e}"))?;
            let enc = res.encryption.clone();
            (res, ctx.blob.clone(), enc)
        }
        RecryptDirection::Decrypt => {
            let plain = ctx.plain_blob.as_ref().expect("plain_blob checked present");
            let res = plain
                .put(&new_blob_id, plaintext.stream)
                .await
                .map_err(|e| format!("write plaintext blob: {e}"))?;
            (res, plain.clone(), None)
        }
    };

    let _ = put_result;
    let sidecar = SidecarMeta {
        bucket: record.bucket.clone(),
        key: record.key.clone(),
        size: record.size,
        etag: record.etag.clone(),
        content_type: record.content_type.clone(),
        last_modified: record.last_modified.to_rfc3339(),
        metadata: record.metadata.clone(),
        encryption: encryption.clone(),
        compression: None,
        version_id: version_id.clone(),
        composite: None,
    };
    write_store
        .write_sidecar(&new_blob_id, &sidecar)
        .await
        .map_err(|e| format!("write sidecar: {e}"))?;

    let (algo, key_id) = match &encryption {
        Some(e) => (Some(e.algorithm.as_str()), Some(e.key_id.as_str())),
        None => (None, None),
    };
    let swapped = ctx
        .metadata
        .update_object_encryption_cas(
            &record.bucket,
            &record.key,
            version_id.as_deref(),
            &old_blob_id,
            &new_blob_id,
            algo,
            key_id,
        )
        .await
        .map_err(|e| format!("CAS row swap: {e}"))?;

    if swapped {
        // The old blob is now orphaned. On a single node reclaim it eagerly
        // (encrypting store cascades composite parts, but candidates are never
        // composite). In a cluster do NOT delete it here: the re-encrypted row
        // (blob_id = new) propagates via anti-entropy and each peer then pulls
        // the new blob by blob repair; reclaiming the old blob is left to the
        // same grace-bounded anti-entropy GC that reclaims every other orphan,
        // so re-encryption never races repair/GC with an out-of-band delete.
        // TECHDEBT(TD-021): the old blob lingers until the next GC pass — a
        // transient extra copy per re-encrypted object on every node.
        // Content and lock state converge on independent LWW dimensions
        // (content_updated_at vs lock_updated_at), so a concurrent lock op can
        // no longer revert blob_id to the old value.
        if !ctx.clustered {
            let _ = ctx.blob.delete(&old_blob_id).await;
        }
        Ok(true)
    } else {
        // A concurrent client write replaced the blob first: discard ours. The
        // freshly written blob was never referenced by a committed row (the CAS
        // matched zero rows) and never propagated, so deleting it is always safe.
        let _ = write_store.delete(&new_blob_id).await;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_storage::SqliteStore;
    use chrono::Utc;

    fn noop_job(id: &str, mode: &str, n: u64) -> MaintenanceJob {
        let now = Utc::now();
        MaintenanceJob {
            id: id.to_string(),
            job_type: "noop".to_string(),
            status: MaintenanceJobStatus::Pending.as_db().to_string(),
            mode: mode.to_string(),
            params: serde_json::json!({ "n": n }),
            total: n,
            done: 0,
            rate: 0.0,
            last_error: None,
            created_at: now,
            updated_at: now,
            started_at: None,
            finished_at: None,
        }
    }

    #[tokio::test]
    async fn tick_runs_noop_to_completion_and_toggles_drain() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (tx, rx) = watch::channel(false);

        store.create_job(&noop_job("j1", "maintenance", 3)).await.unwrap();

        // First tick starts and completes the (tiny, no-delay) job.
        maintenance_tick(&store, None, None, &tx).await;
        let j = store.get_job("j1").await.unwrap().unwrap();
        assert_eq!(j.status, "completed");
        assert_eq!(j.done, 3);

        // A maintenance-mode job drained while active; now that it is terminal a
        // follow-up tick releases the drain.
        maintenance_tick(&store, None, None, &tx).await;
        assert!(!*rx.borrow(), "drain released after the job finished");
    }

    #[tokio::test]
    async fn live_mode_job_never_drains() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (tx, rx) = watch::channel(false);
        store.create_job(&noop_job("j1", "live", 1)).await.unwrap();
        maintenance_tick(&store, None, None, &tx).await;
        assert!(!*rx.borrow(), "live-mode jobs never drain the S3 API");
    }

    #[tokio::test]
    async fn paused_job_keeps_progress_and_does_not_complete() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (tx, _rx) = watch::channel(false);

        // Start the job, then immediately pause: it must not run to completion.
        let mut job = noop_job("j1", "live", 100);
        job.params = serde_json::json!({ "n": 100, "delay_ms": 50 });
        store.create_job(&job).await.unwrap();
        store
            .set_job_status("j1", MaintenanceJobStatus::Running, None)
            .await
            .unwrap();
        store
            .set_job_status("j1", MaintenanceJobStatus::Paused, None)
            .await
            .unwrap();

        maintenance_tick(&store, None, None, &tx).await;
        let j = store.get_job("j1").await.unwrap().unwrap();
        assert_eq!(j.status, "paused");
        assert!(j.done < 100);
    }

    #[tokio::test]
    async fn unknown_job_type_fails_cleanly() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (tx, _rx) = watch::channel(false);
        let mut job = noop_job("j1", "live", 1);
        job.job_type = "does-not-exist".to_string();
        store.create_job(&job).await.unwrap();

        maintenance_tick(&store, None, None, &tx).await;
        let j = store.get_job("j1").await.unwrap().unwrap();
        assert_eq!(j.status, "failed");
        assert!(j.last_error.unwrap().contains("unknown job type"));
    }

    #[tokio::test]
    async fn encrypt_job_without_recrypt_ctx_fails() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (tx, _rx) = watch::channel(false);
        let mut job = noop_job("j1", "live", 1);
        job.job_type = "encrypt".to_string();
        store.create_job(&job).await.unwrap();
        // No RecryptCtx passed → the job fails cleanly instead of panicking.
        maintenance_tick(&store, None, None, &tx).await;
        assert_eq!(store.get_job("j1").await.unwrap().unwrap().status, "failed");
    }

    #[tokio::test]
    async fn migrate_db_job_without_ctx_fails() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (tx, _rx) = watch::channel(false);
        let mut job = noop_job("j1", "maintenance", 1);
        job.job_type = "migrate-db".to_string();
        job.params = serde_json::json!({ "target": "postgres" });
        store.create_job(&job).await.unwrap();
        // No MigrateDbCtx passed → the job fails cleanly instead of panicking.
        maintenance_tick(&store, None, None, &tx).await;
        assert_eq!(store.get_job("j1").await.unwrap().unwrap().status, "failed");
    }

    #[test]
    fn multipart_etag_detection() {
        assert!(is_multipart_etag("9bb58f26192e4ba00f01e2e7b136bbd8-3"));
        assert!(is_multipart_etag("9bb58f26192e4ba00f01e2e7b136bbd8-12"));
        // Plain single-blob etag (32 hex, no part suffix).
        assert!(!is_multipart_etag("9bb58f26192e4ba00f01e2e7b136bbd8"));
        // Non-numeric / malformed suffixes.
        assert!(!is_multipart_etag("9bb58f26192e4ba00f01e2e7b136bbd8-"));
        assert!(!is_multipart_etag("9bb58f26192e4ba00f01e2e7b136bbd8-x"));
        assert!(!is_multipart_etag("short-3"));
    }
}
