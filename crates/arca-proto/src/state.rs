//! Shared application state for the Axum router.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::collections::HashMap;
use std::time::Instant;

use arca_core::store::{AuditStore, BlobStore, ConnectorRegistry, CredentialStore, GrantStore, MetadataStore, MetricsStore, NotificationStore, PresignedUrlStore, ServerConfigStore, SsecBlobOps, TeamStore, UserStore};
use arca_core::store::audit::AuditEntry;

use crate::metrics::MetricsRegistry;

/// Data sent through the audit channel for batched writing.
#[derive(Clone)]
pub struct AuditData {
    pub entry: AuditEntry,
}

/// Application state shared across all handlers.
#[derive(Clone)]
pub struct AppState {
    pub metadata: Arc<dyn MetadataStore>,
    /// Primary blob store — EncryptingBlobStore when a master key is configured
    /// (handles mixed-mode reads: auto-detects encrypted vs plain blobs),
    /// plain FsBlobStore otherwise.
    pub blob: Arc<dyn BlobStore>,
    /// Plain (non-encrypting) blob store. Present when a master key is configured,
    /// used for writes to buckets without encryption.
    pub plain_blob: Option<Arc<dyn BlobStore>>,
    /// SSE-C blob store. Always present (wraps FsBlobStore).
    pub ssec_blob: Option<Arc<dyn SsecBlobOps>>,
    pub credentials: Arc<dyn CredentialStore>,
    pub users: Arc<dyn UserStore>,
    pub teams: Arc<dyn TeamStore>,
    pub grants: Arc<dyn GrantStore>,
    pub server_config: Arc<dyn ServerConfigStore>,
    pub domain: Option<String>,
    /// S3 region from config file (when set, locked and read-only from console).
    pub config_region: Option<String>,
    pub started_at: std::time::Instant,
    pub version: String,
    pub tls_enabled: bool,
    /// Whether audit logging is enabled.
    pub audit_enabled: bool,
    /// Audit log retention days from config file (None = console can set it).
    pub config_audit_retention_days: Option<u32>,
    /// Whether metrics collection is enabled.
    pub metrics_enabled: bool,
    /// Metrics retention days from config file (None = console can set it).
    pub config_metrics_retention_days: Option<u32>,
    /// Notification event retention days from config file (None = console can set it).
    pub config_notification_retention_days: Option<u32>,
    /// Whether server-side encryption is enabled by default for new objects.
    pub encryption_enabled: bool,
    /// KMS provider: "local" (config file), "vault" (Vault/OpenBAO), or None.
    pub kms_provider: Option<String>,
    /// KMS endpoint URL (only when kms_provider = "vault").
    pub kms_endpoint: Option<String>,
    /// Metadata backend: "sqlite" or "postgres".
    pub metadata_backend: String,
    /// Data directories (for filesystem stats). Multiple entries for multi-volume setups.
    pub data_dirs: Vec<PathBuf>,
    /// Audit log store (for writing audit entries).
    pub audit_store: Option<Arc<dyn AuditStore>>,
    /// Metrics snapshot store (for writing periodic gauge snapshots).
    pub metrics_store: Option<Arc<dyn MetricsStore>>,
    /// In-memory metrics registry (counters, histograms, active connections).
    pub metrics_registry: Option<Arc<MetricsRegistry>>,
    /// Per-IP rate limiter (None = disabled).
    pub ip_rate_limiter: Option<std::sync::Arc<crate::middleware::rate_limit::KeyedRateLimiter>>,
    /// Per-credential rate limiter (None = disabled).
    pub credential_rate_limiter: Option<std::sync::Arc<crate::middleware::rate_limit::KeyedRateLimiter>>,
    /// Maximum request body size in bytes (0 = unlimited).
    pub max_body_size: u64,
    /// Maximum number of HTTP headers per request.
    pub max_header_count: u32,
    /// Maximum total size of user metadata headers (`x-amz-meta-*`) in bytes.
    pub max_metadata_size: u32,
    /// Drain mode receiver — when true, health endpoint returns 503.
    pub draining: tokio::sync::watch::Receiver<bool>,
    /// Notification event channel sender (None = notifications disabled).
    pub notification_tx: Option<tokio::sync::mpsc::Sender<arca_core::s3::notification::S3Event>>,
    /// Notification event store (for persisting events and console log viewer).
    pub notification_store: Option<Arc<dyn NotificationStore>>,
    /// Connector registry for testing notification destinations from admin API.
    pub connector_registry: Option<Arc<ConnectorRegistry>>,
    /// Presigned URL tracking store (for visibility in console).
    pub presigned_url_store: Option<Arc<dyn PresignedUrlStore>>,
    /// Cache for per-bucket encryption config lookups (bucket -> (has_encryption, expires_at)).
    /// Avoids a DB query on every PUT/UploadPart when global encryption is disabled.
    pub bucket_encryption_cache: Arc<RwLock<HashMap<String, (bool, Instant)>>>,
    /// Audit log channel sender for batched writes (None = audit disabled or using legacy per-request inserts).
    pub audit_tx: Option<tokio::sync::mpsc::Sender<AuditData>>,
}

impl AppState {
    /// Emit an S3 event to the notification channel (non-blocking, fire-and-forget).
    /// If the channel is full, the event is silently dropped (S3 best-effort semantics).
    pub fn emit_event(&self, event: arca_core::s3::notification::S3Event) {
        if let Some(ref tx) = self.notification_tx {
            let _ = tx.try_send(event);
        }
    }

    /// Returns the appropriate blob store for writing to a specific bucket.
    ///
    /// Checks per-bucket encryption config and the global default to decide
    /// whether to write through the encrypting store or the plain store.
    /// Results are cached for 30 seconds to avoid a DB query on every write.
    pub async fn blob_for_write(&self, bucket: &str) -> Arc<dyn BlobStore> {
        let should_encrypt = if self.encryption_enabled {
            true
        } else if self.plain_blob.is_none() {
            // No master key configured — per-bucket encryption is impossible.
            false
        } else {
            // Check cache first.
            let now = Instant::now();
            let cached = self.bucket_encryption_cache.read().ok()
                .and_then(|cache| cache.get(bucket).copied())
                .filter(|(_, expires)| *expires > now)
                .map(|(val, _)| val);

            if let Some(val) = cached {
                val
            } else {
                let val = matches!(
                    self.metadata.get_bucket_config(bucket, "encryption_algorithm").await,
                    Ok(Some(_))
                );
                if let Ok(mut cache) = self.bucket_encryption_cache.write() {
                    cache.insert(bucket.to_string(), (val, now + std::time::Duration::from_secs(30)));
                }
                val
            }
        };

        if should_encrypt {
            self.blob.clone()
        } else {
            self.plain_blob.clone().unwrap_or_else(|| self.blob.clone())
        }
    }

    /// Invalidate the per-bucket encryption cache entry (called when bucket
    /// encryption config changes).
    pub fn invalidate_bucket_encryption_cache(&self, bucket: &str) {
        if let Ok(mut cache) = self.bucket_encryption_cache.write() {
            cache.remove(bucket);
        }
    }

    /// Send an audit entry through the batched channel (non-blocking).
    /// Falls back to direct insert if channel is not available.
    pub fn send_audit(&self, entry: AuditEntry) {
        if let Some(ref tx) = self.audit_tx {
            let _ = tx.try_send(AuditData { entry });
        } else if let Some(ref audit_store) = self.audit_store {
            // Legacy fallback: direct insert via tokio::spawn.
            let audit = audit_store.clone();
            tokio::spawn(async move {
                if let Err(e) = audit.insert_audit_entry(&entry).await {
                    tracing::warn!(error = %e, "Failed to write audit log entry");
                }
            });
        }
    }
}

/// Spawn the dedicated audit batch writer task.
/// Returns the sender end of the channel for AppState.
pub fn spawn_audit_writer(
    audit_store: Arc<dyn AuditStore>,
) -> tokio::sync::mpsc::Sender<AuditData> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AuditData>(10_000);

    tokio::spawn(async move {
        let mut batch: Vec<AuditEntry> = Vec::with_capacity(128);

        loop {
            // Wait for the first entry or channel close.
            match rx.recv().await {
                Some(data) => batch.push(data.entry),
                None => break, // channel closed, shut down
            }

            // Drain up to 127 more entries without waiting.
            while batch.len() < 128 {
                match rx.try_recv() {
                    Ok(data) => batch.push(data.entry),
                    Err(_) => break,
                }
            }

            // Flush the batch.
            if !batch.is_empty() {
                if let Err(e) = audit_store.insert_audit_entries_batch(&batch).await {
                    tracing::warn!(error = %e, count = batch.len(), "Failed to write audit batch");
                }
                batch.clear();
            }
        }

        // Flush remaining on shutdown.
        if !batch.is_empty() {
            let _ = audit_store.insert_audit_entries_batch(&batch).await;
        }
    });

    tx
}
