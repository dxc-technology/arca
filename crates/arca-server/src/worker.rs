//! Background workers for periodic tasks.
//!
//! Provides a simple `BackgroundWorker` abstraction for spawning periodic
//! tasks, used by the metrics snapshot, retention purge, and lifecycle
//! evaluation workers.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use arca_core::store::audit::{AuditEntry, AuditStore};
use arca_core::store::blob::BlobStore;
use arca_core::store::metadata::MetadataStore;
use arca_core::store::metrics::MetricsStore;
use arca_core::store::server_config::ServerConfigStore;
use arca_proto::AppState;

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
    let notification_store: Option<Arc<dyn arca_core::store::NotificationStore>> =
        state.notification_store.clone();
    let presigned_url_store: Option<Arc<dyn arca_core::store::PresignedUrlStore>> =
        state.presigned_url_store.clone();
    let server_config: Arc<dyn ServerConfigStore> = state.server_config.clone();
    let config_audit_ret = state.config_audit_retention_days;
    let config_metrics_ret = state.config_metrics_retention_days;
    let config_notif_ret = state.config_notification_retention_days;

    BackgroundWorker::spawn_periodic(
        "retention-purge",
        Duration::from_secs(3600), // once per hour
        move || {
            let audit_store = audit_store.clone();
            let metrics_store = metrics_store.clone();
            let notification_store = notification_store.clone();
            let presigned_url_store = presigned_url_store.clone();
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

                // Resolve effective notification retention
                let notif_days = resolve_retention(
                    config_notif_ret,
                    server_config.as_ref(),
                    "notification_retention_days",
                    7,
                )
                .await;

                if notif_days > 0 {
                    if let Some(ref store) = notification_store {
                        let cutoff = chrono::Utc::now()
                            - chrono::Duration::days(notif_days as i64);
                        match store.purge_notification_events(cutoff).await {
                            Ok(n) if n > 0 => {
                                tracing::info!(
                                    purged = n,
                                    "retention: purged notification events"
                                )
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(
                                    error = %e,
                                    "retention: failed to purge notification events"
                                )
                            }
                        }
                    }
                }

                // Purge expired presigned URL tracking records
                if let Some(ref store) = presigned_url_store {
                    match store.purge_expired_presigned_urls().await {
                        Ok(n) if n > 0 => {
                            tracing::info!(
                                purged = n,
                                "retention: purged expired presigned URL records"
                            )
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "retention: failed to purge presigned URL records"
                            )
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

/// Spawn the lifecycle evaluation worker.
///
/// Periodically evaluates lifecycle rules on all buckets: expires objects,
/// deletes noncurrent versions, and aborts stale multipart uploads.
pub fn spawn_lifecycle_worker(
    state: &AppState,
    config_interval_seconds: Option<u64>,
) -> BackgroundWorker {
    let metadata = state.metadata.clone();
    let blob = state.blob.clone();
    let audit_store: Option<Arc<dyn AuditStore>> = state.audit_store.clone();

    // Resolve interval: TOML config > DB setting > 3600s (1 hour) default.
    // The interval is fixed at spawn time; changing the DB setting requires
    // a server restart to take effect.
    let interval_secs = config_interval_seconds.unwrap_or(3600);
    let interval = Duration::from_secs(interval_secs);

    BackgroundWorker::spawn_periodic(
        "lifecycle-evaluator",
        interval,
        move || {
            let metadata = metadata.clone();
            let blob = blob.clone();
            let audit_store = audit_store.clone();
            async move {
                evaluate_lifecycle_rules(
                    metadata.as_ref(),
                    blob.as_ref(),
                    audit_store.as_deref(),
                )
                .await;
            }
        },
    )
}

/// Maximum objects processed per rule per evaluation cycle.
const LIFECYCLE_BATCH_SIZE: u32 = 100;

/// Check if an object is protected by Object Lock (retention or legal hold).
fn is_object_locked(obj: &arca_core::types::ObjectRecord) -> bool {
    if obj.legal_hold_status.as_deref() == Some("ON") {
        return true;
    }
    if let (Some(_mode), Some(until)) = (&obj.retention_mode, &obj.retain_until_date) {
        if chrono::Utc::now() < *until {
            return true;
        }
    }
    false
}

/// Evaluate lifecycle rules for all buckets.
async fn evaluate_lifecycle_rules(
    metadata: &dyn MetadataStore,
    blob: &dyn BlobStore,
    audit_store: Option<&dyn AuditStore>,
) {
    let buckets = match metadata.list_buckets().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, "lifecycle: failed to list buckets");
            return;
        }
    };

    for bucket_info in &buckets {
        let bucket = &bucket_info.name;

        // Read lifecycle rules for this bucket
        let config_json = match metadata.get_bucket_config(bucket, "lifecycle_rules").await {
            Ok(Some(json)) => json,
            Ok(None) => continue, // No lifecycle rules
            Err(e) => {
                tracing::warn!(error = %e, bucket = %bucket, "lifecycle: failed to read config");
                continue;
            }
        };

        let config: arca_core::s3::lifecycle::LifecycleConfiguration =
            match serde_json::from_str(&config_json) {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!(error = %e, bucket = %bucket, "lifecycle: corrupted config JSON");
                    continue;
                }
            };

        // Determine bucket versioning state once per bucket
        let is_versioned = matches!(
            metadata.get_bucket_config(bucket, "versioning").await,
            Ok(Some(ref v)) if v == "Enabled" || v == "Suspended"
        );

        let now = chrono::Utc::now();

        for rule in &config.rules {
            if rule.status != arca_core::s3::lifecycle::RuleStatus::Enabled {
                continue;
            }

            let prefix = rule.filter.prefix();
            let tags: Vec<(String, String)> = rule
                .filter
                .tags()
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

            // Expiration: delete current objects older than N days
            if let Some(ref exp) = rule.expiration {
                let days = match exp {
                    arca_core::s3::lifecycle::Expiration::Days { days } => *days as i64,
                    arca_core::s3::lifecycle::Expiration::Date { date } => {
                        // Calculate days from now to the target date
                        if let Ok(target) = chrono::DateTime::parse_from_rfc3339(date) {
                            let diff = target.signed_duration_since(now);
                            if diff.num_seconds() > 0 {
                                continue; // Date is in the future, skip
                            }
                            0 // Date has passed, expire immediately
                        } else {
                            continue; // Invalid date format, skip rule
                        }
                    }
                    arca_core::s3::lifecycle::Expiration::ExpiredObjectDeleteMarker { .. } => {
                        // Delete marker cleanup is a separate operation, skip normal expiration
                        continue;
                    }
                };
                let cutoff = now - chrono::Duration::days(days);
                match metadata
                    .list_expired_objects(bucket, prefix, &tags, cutoff, None, LIFECYCLE_BATCH_SIZE)
                    .await
                {
                    Ok(objects) => {
                        for obj in &objects {
                            expire_object(metadata, blob, audit_store, bucket, obj, is_versioned)
                                .await;
                        }
                        if !objects.is_empty() {
                            tracing::info!(
                                bucket = %bucket,
                                rule = %rule.id,
                                count = objects.len(),
                                "lifecycle: expired objects"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            bucket = %bucket,
                            rule = %rule.id,
                            "lifecycle: failed to list expired objects"
                        );
                    }
                }
            }

            // NoncurrentVersionExpiration: hard-delete old versions
            if let Some(ref nve) = rule.noncurrent_version_expiration {
                let cutoff = now - chrono::Duration::days(nve.noncurrent_days as i64);
                match metadata
                    .list_noncurrent_expired_versions(
                        bucket,
                        prefix,
                        cutoff,
                        None,
                        LIFECYCLE_BATCH_SIZE,
                    )
                    .await
                {
                    Ok(versions) => {
                        for obj in &versions {
                            expire_noncurrent_version(metadata, blob, audit_store, bucket, obj)
                                .await;
                        }
                        if !versions.is_empty() {
                            tracing::info!(
                                bucket = %bucket,
                                rule = %rule.id,
                                count = versions.len(),
                                "lifecycle: expired noncurrent versions"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            bucket = %bucket,
                            rule = %rule.id,
                            "lifecycle: failed to list noncurrent versions"
                        );
                    }
                }
            }

            // AbortIncompleteMultipartUpload
            if let Some(ref abort) = rule.abort_incomplete_multipart_upload {
                let cutoff = now - chrono::Duration::days(abort.days_after_initiation as i64);
                match metadata
                    .list_stale_multipart_uploads(bucket, cutoff, LIFECYCLE_BATCH_SIZE)
                    .await
                {
                    Ok(uploads) => {
                        for upload in &uploads {
                            abort_stale_upload(metadata, blob, audit_store, bucket, upload).await;
                        }
                        if !uploads.is_empty() {
                            tracing::info!(
                                bucket = %bucket,
                                rule = %rule.id,
                                count = uploads.len(),
                                "lifecycle: aborted stale multipart uploads"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            bucket = %bucket,
                            rule = %rule.id,
                            "lifecycle: failed to list stale uploads"
                        );
                    }
                }
            }
        }
    }
}

/// Expire a single current object.
async fn expire_object(
    metadata: &dyn MetadataStore,
    blob: &dyn BlobStore,
    audit_store: Option<&dyn AuditStore>,
    bucket: &str,
    obj: &arca_core::types::ObjectRecord,
    is_versioned: bool,
) {
    // For versioned buckets, delete_object creates a delete marker (no blob cleanup).
    // For unversioned, it hard-deletes and returns the record for blob cleanup.
    match metadata.delete_object(bucket, &obj.key).await {
        Ok(Some(deleted)) => {
            if !is_versioned && !deleted.is_delete_marker && !deleted.blob_id.0.is_empty() {
                if let Err(e) = blob.delete(&deleted.blob_id).await {
                    tracing::warn!(
                        error = %e,
                        blob_id = %deleted.blob_id,
                        "lifecycle: failed to delete blob"
                    );
                }
            }
        }
        Ok(None) => {} // Already gone
        Err(e) => {
            tracing::warn!(
                error = %e,
                bucket = %bucket,
                key = %obj.key,
                "lifecycle: failed to delete object"
            );
            return;
        }
    }

    write_lifecycle_audit(audit_store, "Lifecycle::ExpireObject", bucket, &obj.key, obj.version_id.as_deref()).await;
}

/// Hard-delete a noncurrent object version.
async fn expire_noncurrent_version(
    metadata: &dyn MetadataStore,
    blob: &dyn BlobStore,
    audit_store: Option<&dyn AuditStore>,
    bucket: &str,
    obj: &arca_core::types::ObjectRecord,
) {
    let version_id = match &obj.version_id {
        Some(v) => v.as_str(),
        None => return, // Noncurrent versions always have a version_id
    };

    // Skip locked objects (Object Lock enforcement)
    if is_object_locked(obj) {
        tracing::debug!(
            bucket = %bucket,
            key = %obj.key,
            version_id = %version_id,
            "lifecycle: skipping locked noncurrent version"
        );
        return;
    }

    match metadata.delete_object_version(bucket, &obj.key, version_id).await {
        Ok(Some(deleted)) => {
            if !deleted.blob_id.0.is_empty() {
                if let Err(e) = blob.delete(&deleted.blob_id).await {
                    tracing::warn!(
                        error = %e,
                        blob_id = %deleted.blob_id,
                        "lifecycle: failed to delete noncurrent blob"
                    );
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                error = %e,
                bucket = %bucket,
                key = %obj.key,
                version_id = %version_id,
                "lifecycle: failed to delete noncurrent version"
            );
            return;
        }
    }

    write_lifecycle_audit(
        audit_store,
        "Lifecycle::ExpireNoncurrentVersion",
        bucket,
        &obj.key,
        Some(version_id),
    )
    .await;
}

/// Abort a stale multipart upload and clean up part blobs.
async fn abort_stale_upload(
    metadata: &dyn MetadataStore,
    blob: &dyn BlobStore,
    audit_store: Option<&dyn AuditStore>,
    bucket: &str,
    upload: &arca_core::types::MultipartUploadRecord,
) {
    match metadata.delete_multipart_upload(&upload.upload_id).await {
        Ok(parts) => {
            for part in &parts {
                if let Err(e) = blob.delete(&part.blob_id).await {
                    tracing::warn!(
                        error = %e,
                        blob_id = %part.blob_id,
                        "lifecycle: failed to delete part blob"
                    );
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                upload_id = %upload.upload_id,
                "lifecycle: failed to abort multipart upload"
            );
            return;
        }
    }

    write_lifecycle_audit(
        audit_store,
        "Lifecycle::AbortMultipartUpload",
        bucket,
        &upload.key,
        None,
    )
    .await;
}

/// Write an audit entry for a lifecycle action.
async fn write_lifecycle_audit(
    audit_store: Option<&dyn AuditStore>,
    operation: &str,
    bucket: &str,
    key: &str,
    version_id: Option<&str>,
) {
    if let Some(store) = audit_store {
        let entry = AuditEntry {
            id: 0,
            timestamp: chrono::Utc::now(),
            request_id: uuid::Uuid::new_v4().to_string(),
            operation: operation.to_string(),
            bucket: Some(bucket.to_string()),
            key: Some(key.to_string()),
            version_id: version_id.map(|v| v.to_string()),
            user_id: Some("system".to_string()),
            access_key_id: None,
            source_ip: None,
            http_method: String::new(),
            http_status: 204,
            error_code: None,
            bytes_sent: 0,
            bytes_received: 0,
            duration_ms: 0,
            user_agent: Some("arca-lifecycle-worker".to_string()),
        };
        if let Err(e) = store.insert_audit_entry(&entry).await {
            tracing::warn!(error = %e, "lifecycle: failed to write audit entry");
        }
    }
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

// ── Notification delivery worker ──

use arca_core::s3::notification::{
    matches_event, matches_filter, NotificationConfiguration, S3Event, S3EventMessage,
};
use arca_core::store::connector::{ConnectorRegistry, NotificationConnector};
use arca_core::store::notification::{NotificationEventRecord, NotificationStore};
use crate::config::NotificationsConfig;

/// Spawn the notification delivery worker.
///
/// Reads S3 events from the mpsc channel, matches them against per-bucket
/// notification configurations, persists records, and delivers events via
/// the appropriate connector from the registry.
pub fn spawn_notification_worker(
    mut rx: tokio::sync::mpsc::Receiver<S3Event>,
    metadata: Arc<dyn MetadataStore>,
    notification_store: Arc<dyn NotificationStore>,
    connector_registry: Arc<ConnectorRegistry>,
    region: String,
    config: NotificationsConfig,
) -> BackgroundWorker {
    let handle = tokio::spawn(async move {
        // Simple in-memory cache for notification configs (bucket -> Option<config>).
        let mut config_cache: std::collections::HashMap<
            String,
            (std::time::Instant, Option<NotificationConfiguration>),
        > = std::collections::HashMap::new();
        let cache_ttl = Duration::from_secs(60);

        while let Some(event) = rx.recv().await {
            // Look up notification config for this bucket (cached).
            let notif_config = {
                let now = std::time::Instant::now();
                let cached = config_cache.get(&event.bucket);
                if let Some((ts, cfg)) = cached {
                    if now.duration_since(*ts) < cache_ttl {
                        cfg.clone()
                    } else {
                        let cfg = fetch_notification_config(metadata.as_ref(), &event.bucket).await;
                        config_cache.insert(event.bucket.clone(), (now, cfg.clone()));
                        cfg
                    }
                } else {
                    let cfg = fetch_notification_config(metadata.as_ref(), &event.bucket).await;
                    config_cache.insert(event.bucket.clone(), (now, cfg.clone()));
                    cfg
                }
            };

            let notif_config = match notif_config {
                Some(c) if !c.is_empty() => c,
                _ => continue, // No notification config for this bucket
            };

            // Check each destination config for a match.
            for dest in notif_config.all_configs() {
                if !dest.enabled {
                    continue;
                }
                let event_matches = dest.events.iter().any(|pat| matches_event(&event.event_name, pat));
                if !event_matches {
                    continue;
                }
                if !matches_filter(&event.key, &dest.filter) {
                    continue;
                }

                // Look up the connector for this destination type.
                let connector = match connector_registry.get(&dest.connector_type) {
                    Some(c) => Arc::clone(c),
                    None => {
                        tracing::warn!(
                            connector_type = %dest.connector_type,
                            "notification: connector not available, skipping delivery"
                        );
                        continue;
                    }
                };

                // Build the full event record
                let record = event.to_record(&dest.id, &region);
                let message = S3EventMessage {
                    records: vec![record],
                };
                let payload = match serde_json::to_string(&message) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, "notification: failed to serialize event");
                        continue;
                    }
                };

                let event_record = NotificationEventRecord {
                    id: uuid::Uuid::new_v4().to_string(),
                    bucket: event.bucket.clone(),
                    key: event.key.clone(),
                    event_name: event.event_name.clone(),
                    event_time: event.timestamp,
                    payload: payload.clone(),
                    destination_url: dest.arn.clone(),
                    configuration_id: dest.id.clone(),
                    delivery_status: "pending".to_string(),
                    delivery_attempts: 0,
                    last_error: None,
                    created_at: chrono::Utc::now(),
                    connector_type: dest.connector_type.to_string(),
                };

                // Persist the event record
                if let Err(e) = notification_store.insert_notification_event(&event_record).await {
                    tracing::warn!(error = %e, "notification: failed to persist event record");
                    continue;
                }

                // Deliver asynchronously via the connector with retry logic
                let dest_url = dest.arn.clone();
                let properties = dest.properties.clone();
                let store = notification_store.clone();
                let event_id = event_record.id.clone();
                let max_retries = config.max_retries;
                let retry_base = config.retry_base_seconds;

                tokio::spawn(async move {
                    deliver_with_retry(
                        connector.as_ref(),
                        &dest_url,
                        &payload,
                        &properties,
                        &store,
                        &event_id,
                        max_retries,
                        retry_base,
                    )
                    .await;
                });
            }
        }

        tracing::info!("notification worker: channel closed, shutting down");
    });

    BackgroundWorker { handle }
}

/// Fetch and deserialize the notification configuration for a bucket.
async fn fetch_notification_config(
    metadata: &dyn MetadataStore,
    bucket: &str,
) -> Option<NotificationConfiguration> {
    match metadata.get_bucket_config(bucket, "notification_configuration").await {
        Ok(Some(json_str)) => match serde_json::from_str(&json_str) {
            Ok(config) => Some(config),
            Err(e) => {
                tracing::warn!(error = %e, bucket = %bucket, "notification: corrupted config");
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = %e, bucket = %bucket, "notification: failed to read config");
            None
        }
    }
}

/// Deliver a notification event via a connector with retry logic.
async fn deliver_with_retry(
    connector: &dyn NotificationConnector,
    destination: &str,
    payload: &str,
    properties: &std::collections::HashMap<String, String>,
    store: &Arc<dyn NotificationStore>,
    event_id: &str,
    max_retries: u32,
    retry_base_seconds: u64,
) {
    let mut attempts = 0u32;

    loop {
        attempts += 1;
        let result = connector.deliver(destination, payload, properties).await;

        if result.success {
            let _ = store
                .update_notification_event_status(event_id, "delivered", attempts, None)
                .await;
            tracing::debug!(
                destination = %destination,
                connector = connector.name(),
                attempts,
                status = %result.status_info,
                "notification: delivered"
            );
            return;
        }

        let error_msg = result.error.unwrap_or_else(|| result.status_info.clone());

        if attempts >= max_retries {
            let _ = store
                .update_notification_event_status(event_id, "failed", attempts, Some(&error_msg))
                .await;
            tracing::warn!(
                destination = %destination,
                connector = connector.name(),
                attempts,
                error = %error_msg,
                "notification: delivery failed permanently"
            );
            return;
        }

        tracing::debug!(
            destination = %destination,
            connector = connector.name(),
            attempts,
            error = %error_msg,
            "notification: retrying"
        );

        // Exponential backoff
        let delay = Duration::from_secs(retry_base_seconds * (1 << (attempts - 1)));
        tokio::time::sleep(delay).await;
    }
}
