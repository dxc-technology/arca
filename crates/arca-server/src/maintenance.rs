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
use arca_proto::state::AppState;
use tokio::sync::watch;

use crate::worker::BackgroundWorker;

/// How often the worker polls for an active job and advances it.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// Spawns the maintenance-jobs worker. `drain_tx` is the maintenance-drain
/// channel whose receiver lives in [`AppState::maintenance_draining`].
pub fn spawn_maintenance_worker(
    state: &AppState,
    drain_tx: Arc<watch::Sender<bool>>,
) -> BackgroundWorker {
    let store = state.maintenance_store.clone();
    let cluster = state.cluster.clone();

    BackgroundWorker::spawn_periodic("maintenance", TICK_INTERVAL, move || {
        let store = store.clone();
        let cluster = cluster.clone();
        let drain_tx = drain_tx.clone();
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
            maintenance_tick(store.as_ref(), &drain_tx).await;
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
pub async fn maintenance_tick(store: &dyn MaintenanceStore, drain_tx: &watch::Sender<bool>) {
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
                process_job(store, &job).await;
            }
        }
        Some(MaintenanceJobStatus::Running) => process_job(store, &job).await,
        // Paused: idle (drain stays engaged per the reconcile above). Terminal:
        // nothing to do (active_job never returns terminal jobs anyway).
        _ => {}
    }
}

/// Dispatches a job to its processor. New job types plug in here (M2: encrypt /
/// decrypt; M3: migrate-db; M4: migrate-topology).
async fn process_job(store: &dyn MaintenanceStore, job: &MaintenanceJob) {
    let result = match job.job_type.as_str() {
        "noop" => process_noop(store, job).await,
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
        maintenance_tick(&store, &tx).await;
        let j = store.get_job("j1").await.unwrap().unwrap();
        assert_eq!(j.status, "completed");
        assert_eq!(j.done, 3);

        // A maintenance-mode job drained while active; now that it is terminal a
        // follow-up tick releases the drain.
        maintenance_tick(&store, &tx).await;
        assert!(!*rx.borrow(), "drain released after the job finished");
    }

    #[tokio::test]
    async fn live_mode_job_never_drains() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let (tx, rx) = watch::channel(false);
        store.create_job(&noop_job("j1", "live", 1)).await.unwrap();
        maintenance_tick(&store, &tx).await;
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

        maintenance_tick(&store, &tx).await;
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

        maintenance_tick(&store, &tx).await;
        let j = store.get_job("j1").await.unwrap().unwrap();
        assert_eq!(j.status, "failed");
        assert!(j.last_error.unwrap().contains("unknown job type"));
    }
}
