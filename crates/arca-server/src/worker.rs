//! Background workers for periodic tasks.
//!
//! Provides a simple `BackgroundWorker` abstraction for spawning periodic
//! tasks. Designed for reuse by future phases (e.g., Phase 19 lifecycle rules).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use arca_core::store::audit::AuditStore;
use arca_core::store::metrics::MetricsStore;
use arca_core::store::metadata::MetadataStore;
use arca_core::store::server_config::ServerConfigStore;
use arca_proto::AppState;
use arca_proto::handlers::admin_settings;

/// Handle to a background worker task. Aborts the task when dropped.
pub struct BackgroundWorker {
    handle: tokio::task::JoinHandle<()>,
}

impl BackgroundWorker {
    /// Spawn a periodic worker that calls `task` every `interval`.
    pub fn spawn_periodic<F, Fut>(name: &'static str, interval: Duration, task: F) -> Self
    where
        F: Fn() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let handle = tokio::spawn(async move {
            let mut timer = tokio::time::interval(interval);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // Skip the first immediate tick
            timer.tick().await;
            loop {
                timer.tick().await;
                tracing::debug!(worker = name, "running periodic task");
                task().await;
            }
        });
        Self { handle }
    }
}

impl Drop for BackgroundWorker {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Spawn the metrics snapshot worker.
///
/// Periodically reads current stats and writes a `MetricsSnapshot` to the database.
pub fn spawn_metrics_worker(state: &AppState, interval_seconds: u64) -> Option<BackgroundWorker> {
    let metrics_store = state.metrics_store.clone()?;
    let metadata = state.metadata.clone();
    let data_dirs = state.data_dirs.clone();
    let registry = state.metrics_registry.clone();

    let interval = Duration::from_secs(interval_seconds);

    Some(BackgroundWorker::spawn_periodic(
        "metrics-snapshot",
        interval,
        move || {
            let metrics_store = metrics_store.clone();
            let metadata = metadata.clone();
            let data_dirs = data_dirs.clone();
            let registry = registry.clone();
            async move {
                let stats = match metadata.get_stats().await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "metrics worker: failed to get stats");
                        return;
                    }
                };

                let active_connections = registry
                    .as_ref()
                    .map(|r| r.active_connections.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(0);

                // Disk stats from the first data directory
                let (disk_total, disk_available) = disk_stats(&data_dirs);

                let snapshot = arca_core::store::metrics::MetricsSnapshot {
                    id: 0,
                    timestamp: chrono::Utc::now(),
                    bucket_count: stats.bucket_count,
                    object_count: stats.object_count,
                    total_size_bytes: stats.total_size_bytes,
                    disk_total_bytes: disk_total,
                    disk_available_bytes: disk_available,
                    active_connections,
                };

                if let Err(e) = metrics_store.insert_metrics_snapshot(&snapshot).await {
                    tracing::warn!(error = %e, "metrics worker: failed to write snapshot");
                }
            }
        },
    ))
}

/// Spawn the retention purge worker.
///
/// Runs once per hour and deletes old audit log entries and metrics snapshots
/// based on the effective retention settings (TOML > DB > default).
pub fn spawn_retention_worker(state: &AppState) -> BackgroundWorker {
    let audit_store: Option<Arc<dyn AuditStore>> = state.audit_store.clone();
    let metrics_store: Option<Arc<dyn MetricsStore>> = state.metrics_store.clone();
    let server_config: Arc<dyn ServerConfigStore> = state.server_config.clone();
    let config_audit_ret = state.config_audit_retention_days;
    let config_metrics_ret = state.config_metrics_retention_days;

    BackgroundWorker::spawn_periodic(
        "retention-purge",
        Duration::from_secs(3600), // once per hour
        move || {
            let audit_store = audit_store.clone();
            let metrics_store = metrics_store.clone();
            let server_config = server_config.clone();
            async move {
                // Resolve effective audit retention
                let audit_days = resolve_retention(
                    config_audit_ret,
                    server_config.as_ref(),
                    "audit_retention_days",
                    90,
                )
                .await;

                if audit_days > 0 {
                    if let Some(ref store) = audit_store {
                        let cutoff =
                            chrono::Utc::now() - chrono::Duration::days(audit_days as i64);
                        match store.purge_audit_entries(cutoff).await {
                            Ok(n) if n > 0 => {
                                tracing::info!(purged = n, "retention: purged audit log entries")
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "retention: failed to purge audit log")
                            }
                        }
                    }
                }

                // Resolve effective metrics retention
                let metrics_days = resolve_retention(
                    config_metrics_ret,
                    server_config.as_ref(),
                    "metrics_retention_days",
                    30,
                )
                .await;

                if metrics_days > 0 {
                    if let Some(ref store) = metrics_store {
                        let cutoff =
                            chrono::Utc::now() - chrono::Duration::days(metrics_days as i64);
                        match store.purge_metrics_snapshots(cutoff).await {
                            Ok(n) if n > 0 => {
                                tracing::info!(
                                    purged = n,
                                    "retention: purged metrics snapshots"
                                )
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "retention: failed to purge metrics snapshots"
                                )
                            }
                        }
                    }
                }
            }
        },
    )
}

/// Resolve effective retention days: TOML config > DB > default.
async fn resolve_retention(
    config_value: Option<u32>,
    server_config: &dyn ServerConfigStore,
    key: &str,
    default: u32,
) -> u32 {
    if let Some(days) = config_value {
        return days;
    }
    if let Ok(Some(val)) = server_config.get_server_config(key).await {
        if let Ok(days) = val.parse::<u32>() {
            return days;
        }
    }
    default
}

/// Read disk stats from the first data directory using statvfs.
fn disk_stats(data_dirs: &[std::path::PathBuf]) -> (Option<u64>, Option<u64>) {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        if let Some(dir) = data_dirs.first() {
            if let Ok(c_path) = CString::new(dir.to_string_lossy().as_bytes()) {
                let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
                if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } == 0 {
                    let total = stat.f_blocks as u64 * stat.f_frsize as u64;
                    let avail = stat.f_bavail as u64 * stat.f_frsize as u64;
                    return (Some(total), Some(avail));
                }
            }
        }
        (None, None)
    }
    #[cfg(not(unix))]
    {
        let _ = data_dirs;
        (None, None)
    }
}
