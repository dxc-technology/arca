//! Shared application state for the Axum router.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use arca_core::store::{AuditStore, BlobStore, ConnectorRegistry, ControlSnapshotStore, CredentialStore, GrantStore, MetadataStore, MetricsStore, NotificationStore, PresignedUrlStore, RawBlobOps, ReplicationStore, ServerConfigStore, SsecBlobOps, TeamStore, UserStore};
use arca_core::store::audit::AuditEntry;
use arca_core::cluster::ClusterState;

use crate::metrics::MetricsRegistry;

/// Data sent through the audit channel for batched writing.
#[derive(Clone)]
pub struct AuditData {
    pub entry: AuditEntry,
}

/// The control-plane stores BELOW the cluster decorators (i.e. the inner
/// handles captured before the `Cluster*Store` wrappers are applied). The
/// `/cluster/v1/op` receive handler applies replicated control-plane mutations
/// through these so applied ops are NOT re-fanned-out to peers.
///
/// Held as a single `Option<ClusterInnerStores>` on `AppState` because
/// clustering wires either all of these together or none — the bundle makes
/// that invariant a type, not five parallel `Option`s the receive path must
/// re-check.
#[derive(Clone)]
pub struct ClusterInnerStores {
    pub metadata: Arc<dyn MetadataStore>,
    pub credentials: Arc<dyn CredentialStore>,
    pub users: Arc<dyn UserStore>,
    pub grants: Arc<dyn GrantStore>,
    pub teams: Arc<dyn TeamStore>,
    pub server_config: Arc<dyn ServerConfigStore>,
    /// Reads this node's full control snapshot for `GET
    /// /cluster/v1/control-snapshot` (the peer-pull side of the reconcile).
    pub control_snapshot: Arc<dyn ControlSnapshotStore>,
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
    /// Replication journal store (Phase 28). Always present once migrations run.
    pub replication_store: Arc<dyn ReplicationStore>,
    /// Stable identifier for this instance used as the loop-prevention
    /// `x-amz-arca-replication-source` header on outbound replication requests.
    pub replication_source_id: String,
    /// Shared cluster state (Phase 29 HA), `None` when clustering is disabled.
    /// Updated by the membership manager; read by the cluster endpoints, the
    /// admin API, the console dashboard, and the store decorators' quorum gate.
    pub cluster: Option<Arc<ClusterState>>,
    /// Raw, verbatim blob access for the inter-node `/cluster/v1/blob` endpoints
    /// (write_raw / read_raw / sidecar), bypassing the encryption/compression
    /// wrappers. `Some` only when clustering is enabled. Held behind a trait so
    /// arca-proto need not depend on arca-storage's concrete `FsBlobStore`.
    pub cluster_raw_blob: Option<Arc<dyn RawBlobOps>>,
    /// Shared cluster secret (the secret half of the fixed cluster credential),
    /// used by the `cluster_auth` middleware to verify inter-node requests.
    /// `Some` only when clustering is enabled.
    pub cluster_secret: Option<String>,
    /// The control-plane stores below the cluster decorators, applied by the
    /// `/cluster/v1/op` receive handler without re-fan-out. `Some` only when
    /// clustering is enabled (all inner handles are present together).
    pub cluster_inner: Option<ClusterInnerStores>,
    /// Replication journal retention days from the TOML config file
    /// (locks the value, makes it read-only from the console). When absent,
    /// the console can set it via `replication_retention_days` in server_config
    /// with a default fallback; effective value resolves TOML > DB > default.
    pub config_replication_retention_days: Option<u32>,
    /// Hard cap in days for journal rows regardless of status — a safety rail
    /// that prevents unbounded growth when a destination stays offline forever.
    /// TOML-only; not exposed in the console.
    pub replication_journal_max_age_days: u32,
    /// Cache for per-bucket encryption config lookups (bucket -> (has_encryption, expires_at)).
    /// Avoids a DB query on every PUT/UploadPart when global encryption is disabled.
    pub bucket_encryption_cache: Arc<RwLock<HashMap<String, (bool, Instant)>>>,
    /// Cache for per-bucket replication config lookups (bucket -> (has_replication, expires_at)).
    /// Avoids a `bucket_config` query on every write (PutObject / CompleteMultipart /
    /// delete-marker / PutObjectTagging) when the bucket has no replication configured
    /// (the common case). Mirrors `bucket_encryption_cache`.
    pub bucket_replication_cache: Arc<RwLock<HashMap<String, (bool, Instant)>>>,
    /// Optional invalidator for the compression wrapper's per-bucket cache.
    /// When compression is wired, this closure forwards the bucket name to the
    /// `CompressingBlobStore` so its cache entry is dropped on config change.
    pub compression_invalidator: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    /// Audit log channel sender for batched writes (None = audit disabled or using legacy per-request inserts).
    pub audit_tx: Option<tokio::sync::mpsc::Sender<AuditData>>,
    /// Log level from config file (None = console can change it freely).
    pub config_log_level: Option<String>,
    /// Reloader closure for changing the tracing filter at runtime.
    pub log_reloader: Option<Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>>,
}

/// TTL for the per-bucket `bucket_config` lookup caches (encryption, replication):
/// long enough to absorb bursts of writes to the same bucket, short enough that a
/// config change on a path that forgot to invalidate still self-heals quickly.
const BUCKET_CONFIG_CACHE_TTL: Duration = Duration::from_secs(30);

/// Returns the cached boolean for `bucket` if present and not yet expired at `now`.
/// Shared by the per-bucket encryption and replication caches.
fn cached_bucket_flag(
    cache: &RwLock<HashMap<String, (bool, Instant)>>,
    bucket: &str,
    now: Instant,
) -> Option<bool> {
    cache
        .read()
        .ok()
        .and_then(|c| c.get(bucket).copied())
        .filter(|(_, expires)| *expires > now)
        .map(|(val, _)| val)
}

/// Caches `val` for `bucket`, expiring `BUCKET_CONFIG_CACHE_TTL` after `now`.
fn cache_bucket_flag(
    cache: &RwLock<HashMap<String, (bool, Instant)>>,
    bucket: &str,
    val: bool,
    now: Instant,
) {
    if let Ok(mut c) = cache.write() {
        c.insert(bucket.to_string(), (val, now + BUCKET_CONFIG_CACHE_TTL));
    }
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
            // Per-bucket encryption: cache the lookup to avoid a DB query on every write.
            let now = Instant::now();
            if let Some(val) = cached_bucket_flag(&self.bucket_encryption_cache, bucket, now) {
                val
            } else {
                let val = matches!(
                    self.metadata.get_bucket_config(bucket, "encryption_algorithm").await,
                    Ok(Some(_))
                );
                cache_bucket_flag(&self.bucket_encryption_cache, bucket, val, now);
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

    /// Returns whether `bucket` has a replication configuration, caching the
    /// result for 30s to avoid a `bucket_config` read on every write. Mirrors
    /// the per-bucket encryption cache in [`AppState::blob_for_write`]; buckets
    /// without replication (the common case) skip the DB query entirely.
    pub async fn replication_enabled_for(&self, bucket: &str) -> bool {
        let now = Instant::now();
        if let Some(val) = cached_bucket_flag(&self.bucket_replication_cache, bucket, now) {
            return val;
        }
        let val = matches!(
            self.metadata
                .get_bucket_config(bucket, "replication_configuration")
                .await,
            Ok(Some(_))
        );
        cache_bucket_flag(&self.bucket_replication_cache, bucket, val, now);
        val
    }

    /// Invalidate the per-bucket replication cache entry (called when bucket
    /// replication config changes).
    pub fn invalidate_bucket_replication_cache(&self, bucket: &str) {
        if let Ok(mut cache) = self.bucket_replication_cache.write() {
            cache.remove(bucket);
        }
    }

    /// Invalidate the per-bucket compression cache entry in the compression
    /// wrapper (no-op when compression is not configured).
    pub fn invalidate_bucket_compression_cache(&self, bucket: &str) {
        if let Some(f) = &self.compression_invalidator {
            f(bucket);
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

#[cfg(test)]
mod tests {
    use super::{cache_bucket_flag, cached_bucket_flag, BUCKET_CONFIG_CACHE_TTL};
    use std::collections::HashMap;
    use std::sync::RwLock;
    use std::time::{Duration, Instant};

    #[test]
    fn cache_returns_stored_value_within_ttl() {
        let cache = RwLock::new(HashMap::new());
        let now = Instant::now();
        cache_bucket_flag(&cache, "with-config", true, now);
        cache_bucket_flag(&cache, "without-config", false, now);
        assert_eq!(cached_bucket_flag(&cache, "with-config", now), Some(true));
        assert_eq!(cached_bucket_flag(&cache, "without-config", now), Some(false));
    }

    #[test]
    fn cache_misses_for_unknown_bucket() {
        let cache = RwLock::new(HashMap::new());
        assert_eq!(cached_bucket_flag(&cache, "absent", Instant::now()), None);
    }

    #[test]
    fn cache_entry_expires_after_ttl() {
        let cache = RwLock::new(HashMap::new());
        let now = Instant::now();
        cache_bucket_flag(&cache, "b", true, now);
        // Just inside the TTL window: still a hit.
        let inside = now + BUCKET_CONFIG_CACHE_TTL - Duration::from_secs(1);
        assert_eq!(cached_bucket_flag(&cache, "b", inside), Some(true));
        // Past the TTL: a miss, so the next read refreshes from the DB.
        let expired = now + BUCKET_CONFIG_CACHE_TTL + Duration::from_secs(1);
        assert_eq!(cached_bucket_flag(&cache, "b", expired), None);
    }

    #[test]
    fn cache_overwrite_updates_value() {
        let cache = RwLock::new(HashMap::new());
        let now = Instant::now();
        cache_bucket_flag(&cache, "b", true, now);
        // A refresh after a config change flips the cached flag.
        let later = now + Duration::from_secs(5);
        cache_bucket_flag(&cache, "b", false, later);
        assert_eq!(cached_bucket_flag(&cache, "b", later), Some(false));
    }
}
