//! Configuration loading and types.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Top-level configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
    pub encryption: Option<EncryptionConfig>,
    pub monitoring: Option<MonitoringConfig>,
    pub notifications: Option<NotificationsConfig>,
    pub replication: Option<ReplicationConfig>,
    pub lifecycle: Option<LifecycleWorkerConfig>,
    pub cluster: Option<ClusterConfig>,
}

/// Server configuration.
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    /// Optional domain for virtual-hosted-style requests (e.g. "s3.example.com").
    /// When set, requests to `bucket.s3.example.com` are rewritten to `/{bucket}/...`.
    pub domain: Option<String>,
    /// Optional S3 region. When set, this is the instance-wide default and cannot
    /// be changed from the console. When absent, the region can be set via the
    /// Admin API / console (stored in `server_config` table), defaulting to "us-east-1".
    pub region: Option<String>,
    /// Log level filter (default: "info"). Supports tracing-subscriber syntax
    /// (e.g. "debug", "info", "warn", "arca=debug,tower=warn").
    /// When set, this is the startup default and is shown as "config_file" source
    /// in the console. Can be overridden at runtime via the settings API.
    pub log_level: Option<String>,
    /// Optional TLS configuration. When set, the server serves HTTPS.
    pub tls: Option<TlsConfig>,
    /// Request limits and rate limiting configuration.
    pub limits: Option<LimitsConfig>,
    /// In-memory metadata cache configuration.
    pub cache: Option<CacheConfig>,
    /// Tokio runtime configuration (worker threads, blocking pool size).
    /// Read at startup before the runtime is built. Defaults to num_cpus.
    pub runtime: Option<RuntimeConfig>,
    /// HTTP/2 server configuration (per-connection stream limits and windows).
    pub http: Option<HttpConfig>,
}

/// Tokio runtime configuration. All fields are optional; `0` (or absent)
/// means "use num_cpus".
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RuntimeConfig {
    /// Number of tokio worker threads. `0` (default) = num_cpus.
    #[serde(default)]
    pub worker_threads: usize,
    /// Maximum threads in the blocking pool. `0` (default) = num_cpus.
    /// Setting this equal to num_cpus is intentional when AEAD is offloaded
    /// via spawn_blocking — keeps the CPU-bound pool from oversubscribing.
    #[serde(default)]
    pub max_blocking_threads: usize,
}

/// HTTP/2 server tunables. Applied to `hyper_util::server::conn::auto::Builder`
/// once outside the accept loop. Defaults match the values picked for the
/// PBM-style fan-out workload.
#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    /// `SETTINGS_MAX_CONCURRENT_STREAMS` advertised to clients (default: 200).
    #[serde(default = "default_h2_max_concurrent_streams")]
    pub h2_max_concurrent_streams: u32,
    /// HTTP/2 keep-alive ping interval in seconds (default: 30).
    #[serde(default = "default_h2_keep_alive_interval_sec")]
    pub h2_keep_alive_interval_sec: u64,
    /// HTTP/2 keep-alive ping timeout in seconds (default: 20).
    #[serde(default = "default_h2_keep_alive_timeout_sec")]
    pub h2_keep_alive_timeout_sec: u64,
    /// Initial per-stream window size in bytes (default: 2 MiB).
    #[serde(default = "default_h2_initial_stream_window")]
    pub h2_initial_stream_window: u32,
    /// Initial connection window size in bytes (default: 8 MiB).
    #[serde(default = "default_h2_initial_connection_window")]
    pub h2_initial_connection_window: u32,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            h2_max_concurrent_streams: default_h2_max_concurrent_streams(),
            h2_keep_alive_interval_sec: default_h2_keep_alive_interval_sec(),
            h2_keep_alive_timeout_sec: default_h2_keep_alive_timeout_sec(),
            h2_initial_stream_window: default_h2_initial_stream_window(),
            h2_initial_connection_window: default_h2_initial_connection_window(),
        }
    }
}

fn default_h2_max_concurrent_streams() -> u32 {
    200
}

fn default_h2_keep_alive_interval_sec() -> u64 {
    30
}

fn default_h2_keep_alive_timeout_sec() -> u64 {
    20
}

fn default_h2_initial_stream_window() -> u32 {
    2 * 1024 * 1024
}

fn default_h2_initial_connection_window() -> u32 {
    8 * 1024 * 1024
}

/// Request limits and rate limiting configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    /// Maximum request body size in bytes (default: 5,000,000,000 = 5 GB).
    /// Set to 0 for unlimited.
    #[serde(default = "default_max_body_size")]
    pub max_body_size: u64,
    /// Maximum number of HTTP headers per request (default: 100).
    #[serde(default = "default_max_header_count")]
    pub max_header_count: u32,
    /// Maximum total size in bytes of user metadata headers (`x-amz-meta-*`, default: 2048).
    #[serde(default = "default_max_metadata_size")]
    pub max_metadata_size: u32,
    /// Per-credential requests per second (default: 0 = disabled).
    #[serde(default)]
    pub rate_limit_per_second: u32,
    /// Per-credential burst capacity (default: 0 = disabled).
    #[serde(default)]
    pub rate_limit_burst: u32,
    /// Per-IP requests per second (default: 0 = disabled).
    #[serde(default)]
    pub rate_limit_per_ip_per_second: u32,
    /// Per-IP burst capacity (default: 0 = disabled).
    #[serde(default)]
    pub rate_limit_per_ip_burst: u32,
    /// Graceful shutdown drain timeout in seconds (default: 30).
    #[serde(default = "default_drain_timeout")]
    pub drain_timeout_seconds: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_body_size: default_max_body_size(),
            max_header_count: default_max_header_count(),
            max_metadata_size: default_max_metadata_size(),
            rate_limit_per_second: 0,
            rate_limit_burst: 0,
            rate_limit_per_ip_per_second: 0,
            rate_limit_per_ip_burst: 0,
            drain_timeout_seconds: default_drain_timeout(),
        }
    }
}

fn default_max_body_size() -> u64 {
    5_000_000_000 // 5 GB (S3 single PutObject limit)
}

fn default_max_header_count() -> u32 {
    100
}

fn default_max_metadata_size() -> u32 {
    2048 // 2 KB (S3 user metadata limit)
}

fn default_drain_timeout() -> u32 {
    30
}

/// In-memory metadata cache configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct CacheConfig {
    /// Whether the metadata cache is enabled (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum number of cached bucket entries (default: 1000).
    #[serde(default = "default_bucket_cache_size")]
    pub bucket_cache_size: u64,
    /// TTL for bucket cache entries in seconds (default: 60).
    #[serde(default = "default_bucket_cache_ttl")]
    pub bucket_cache_ttl_seconds: u64,
    /// Maximum number of cached object entries (default: 10000).
    #[serde(default = "default_object_cache_size")]
    pub object_cache_size: u64,
    /// TTL for object cache entries in seconds (default: 30).
    #[serde(default = "default_object_cache_ttl")]
    pub object_cache_ttl_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bucket_cache_size: default_bucket_cache_size(),
            bucket_cache_ttl_seconds: default_bucket_cache_ttl(),
            object_cache_size: default_object_cache_size(),
            object_cache_ttl_seconds: default_object_cache_ttl(),
        }
    }
}

fn default_bucket_cache_size() -> u64 {
    1000
}

fn default_bucket_cache_ttl() -> u64 {
    60
}

fn default_object_cache_size() -> u64 {
    10_000
}

fn default_object_cache_ttl() -> u64 {
    30
}

/// TLS configuration for native HTTPS support.
#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    /// Base directory for certificate files. Enables auto-detection or relative paths.
    pub cert_dir: Option<String>,
    /// Certificate chain PEM file (relative to cert_dir, or absolute).
    pub cert_file: Option<String>,
    /// Private key PEM file (relative to cert_dir, or absolute).
    pub key_file: Option<String>,
    /// Client CA PEM for mTLS (relative to cert_dir, or absolute). Always explicit.
    pub ca_file: Option<String>,
}

/// Resolved absolute paths for TLS certificate and key files.
pub struct ResolvedPaths {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    pub ca_path: Option<PathBuf>,
}

impl TlsConfig {
    /// Validate the TLS configuration.
    ///
    /// Valid scenarios:
    /// 1. `cert_dir` only — auto-detect cert and key by scanning PEM headers
    /// 2. `cert_dir` + `cert_file` + `key_file` — relative paths
    /// 3. `cert_file` + `key_file` (both absolute) — files in different dirs
    ///
    /// Invalid: no `cert_dir` and missing either `cert_file` or `key_file`.
    pub fn validate(&self) -> Result<()> {
        match (&self.cert_dir, &self.cert_file, &self.key_file) {
            // Scenario 1: cert_dir only — auto-detect
            (Some(_), None, None) => Ok(()),
            // Scenario 2: cert_dir + both files
            (Some(_), Some(_), Some(_)) => Ok(()),
            // Scenario 3: absolute paths, no cert_dir
            (None, Some(_), Some(_)) => Ok(()),
            // Invalid: cert_dir + only one of cert_file/key_file
            (Some(_), Some(_), None) | (Some(_), None, Some(_)) => {
                bail!("[server.tls] when cert_dir is set with explicit files, both cert_file and key_file are required")
            }
            // Invalid: no cert_dir and missing one or both files
            (None, _, _) => {
                bail!("[server.tls] requires either cert_dir alone, cert_dir + cert_file + key_file, or cert_file + key_file")
            }
        }
    }

    /// Resolve certificate and key file paths.
    ///
    /// For scenario 1 (cert_dir only), delegates to auto-detection.
    /// For scenarios 2 and 3, resolves relative paths against cert_dir.
    pub fn resolve_paths(&self) -> Result<ResolvedPaths> {
        self.validate()?;

        let (cert_path, key_path) = match (&self.cert_dir, &self.cert_file, &self.key_file) {
            // Scenario 1: auto-detect
            (Some(dir), None, None) => {
                crate::tls::detect_pem_files(Path::new(dir))?
            }
            // Scenario 2: cert_dir + relative filenames
            (Some(dir), Some(cert), Some(key)) => {
                let base = Path::new(dir);
                let cert_path = if Path::new(cert).is_absolute() {
                    PathBuf::from(cert)
                } else {
                    base.join(cert)
                };
                let key_path = if Path::new(key).is_absolute() {
                    PathBuf::from(key)
                } else {
                    base.join(key)
                };
                (cert_path, key_path)
            }
            // Scenario 3: absolute paths
            (None, Some(cert), Some(key)) => {
                (PathBuf::from(cert), PathBuf::from(key))
            }
            _ => unreachable!("validate() should catch this"),
        };

        let ca_path = self.ca_file.as_ref().map(|ca| {
            let p = Path::new(ca);
            if p.is_absolute() {
                p.to_path_buf()
            } else if let Some(dir) = &self.cert_dir {
                Path::new(dir).join(ca)
            } else {
                p.to_path_buf()
            }
        });

        Ok(ResolvedPaths {
            cert_path,
            key_path,
            ca_path,
        })
    }
}

/// Storage configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    pub data_dir: String,
    /// Number of 2-char prefix directory levels for blob sharding (1–4, default 2).
    #[serde(default = "default_blob_prefix_depth")]
    pub blob_prefix_depth: u8,
    /// Metadata backend: "sqlite" (default) or "postgres".
    #[serde(default = "default_metadata_backend")]
    pub metadata_backend: String,
    /// PostgreSQL configuration (required when metadata_backend = "postgres").
    pub postgres: Option<PostgresConfig>,
    /// Opt-in single-node blob GC: periodically reclaim orphan blob files.
    /// Off by default. Ignored when clustering is enabled (the anti-entropy
    /// worker reclaims orphans instead). The equivalent on-demand tool is
    /// `arca gc`.
    #[serde(default)]
    pub blob_gc_enabled: bool,
    /// How often the single-node blob GC worker runs, in seconds (default 3600).
    /// The scan walks every blob file and loads the full referenced set, so keep
    /// this generous on large stores.
    #[serde(default = "default_blob_gc_interval_seconds")]
    pub blob_gc_interval_seconds: u64,
    /// Protect blobs written within this many seconds from reclamation (default
    /// 86400). Must exceed the longest in-flight upload — a blob file exists on
    /// disk before its object row is committed, so a smaller window could reclaim
    /// an upload in progress.
    #[serde(default = "default_blob_gc_grace_seconds")]
    pub blob_gc_grace_seconds: u64,
}

fn default_blob_prefix_depth() -> u8 {
    2
}

fn default_metadata_backend() -> String {
    "sqlite".to_string()
}

fn default_blob_gc_interval_seconds() -> u64 {
    3600
}

fn default_blob_gc_grace_seconds() -> u64 {
    86400
}

/// PostgreSQL connection configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct PostgresConfig {
    /// PostgreSQL connection string (e.g. "postgresql://user:pass@host:5432/db").
    pub connection_string: String,
    /// Maximum number of connections in the pool (default: 20).
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
}

fn default_max_connections() -> u32 {
    20
}

impl StorageConfig {
    /// Returns the path to the SQLite database file (`{data_dir}/arca.db`).
    pub fn db_path(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("arca.db")
    }

    /// Returns the path to the blob storage directory (`{data_dir}/blobs`).
    pub fn blobs_dir(&self) -> std::path::PathBuf {
        std::path::Path::new(&self.data_dir).join("blobs")
    }

    /// Interval between single-node blob GC passes (never zero — tokio's timer
    /// requires a positive period).
    pub fn blob_gc_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.blob_gc_interval_seconds.max(1))
    }

    /// Grace window protecting freshly-written blobs from reclamation.
    pub fn blob_gc_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.blob_gc_grace_seconds)
    }

    /// Validates the storage configuration.
    pub fn validate(&self) -> Result<()> {
        match self.metadata_backend.as_str() {
            "sqlite" => Ok(()),
            "postgres" => {
                if self.postgres.is_none() {
                    bail!("[storage.postgres] section is required when metadata_backend = \"postgres\"");
                }
                let pg = self.postgres.as_ref().unwrap();
                if pg.connection_string.is_empty() {
                    bail!("[storage.postgres] connection_string is required");
                }
                Ok(())
            }
            other => bail!("[storage] unknown metadata_backend: \"{other}\" (expected \"sqlite\" or \"postgres\")"),
        }
    }
}

/// Server-side encryption configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct EncryptionConfig {
    /// Whether encryption is enabled by default for new objects.
    #[serde(default)]
    pub enabled: bool,
    /// Base64-encoded 256-bit (32-byte) master key (KEK).
    pub master_key: Option<String>,
    /// Base64-encoded previous master key for key rotation reads.
    pub previous_master_key: Option<String>,
    /// KMS configuration for fetching the master key from Vault/OpenBAO.
    pub kms: Option<KmsConfig>,
}

/// Authentication method for Vault/OpenBAO KMS.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KmsAuthMethod {
    Token,
    Approle,
}

fn default_secret_path() -> String {
    "secret/arca/master-key".to_string()
}

fn default_secret_field() -> String {
    "key".to_string()
}

/// KMS configuration for fetching the master key from HashiCorp Vault or OpenBAO.
#[derive(Debug, Clone, Deserialize)]
pub struct KmsConfig {
    /// Vault/OpenBAO endpoint URL (e.g. "http://vault:8200").
    pub endpoint: String,
    /// KV v2 secret path (default: "secret/arca/master-key").
    /// Auto-normalized: "/data/" segment is inserted after the mount point if missing.
    #[serde(default = "default_secret_path")]
    pub secret_path: String,
    /// Field name within the secret that holds the base64-encoded key (default: "key").
    #[serde(default = "default_secret_field")]
    pub secret_field: String,
    /// Authentication method: "token" or "approle".
    pub auth_method: KmsAuthMethod,
    /// Vault token (required when auth_method = "token").
    pub token: Option<String>,
    /// AppRole role_id (required when auth_method = "approle").
    pub role_id: Option<String>,
    /// AppRole secret_id (required when auth_method = "approle").
    pub secret_id: Option<String>,
    /// Skip TLS certificate verification (development only).
    #[serde(default)]
    pub tls_skip_verify: bool,
    /// CA certificate file for Vault TLS verification.
    pub ca_file: Option<String>,
}

impl KmsConfig {
    pub fn validate(&self) -> Result<()> {
        if self.endpoint.is_empty() {
            bail!("[encryption.kms] endpoint is required");
        }
        match self.auth_method {
            KmsAuthMethod::Token => {
                if self.token.as_ref().map_or(true, |t| t.is_empty()) {
                    bail!("[encryption.kms] token is required when auth_method = \"token\"");
                }
            }
            KmsAuthMethod::Approle => {
                if self.role_id.as_ref().map_or(true, |r| r.is_empty()) {
                    bail!("[encryption.kms] role_id is required when auth_method = \"approle\"");
                }
                if self.secret_id.as_ref().map_or(true, |s| s.is_empty()) {
                    bail!("[encryption.kms] secret_id is required when auth_method = \"approle\"");
                }
            }
        }
        Ok(())
    }
}

impl EncryptionConfig {
    /// Validates the encryption configuration.
    ///
    /// Either `master_key` or `kms` is required whenever the `[encryption]`
    /// section is present (regardless of `enabled`), because per-bucket
    /// encryption needs the key even when the global default is off.
    /// They are mutually exclusive.
    pub fn validate(&self) -> Result<()> {
        match (&self.master_key, &self.kms) {
            (Some(_), Some(_)) => {
                bail!("[encryption] master_key and kms are mutually exclusive — use one or the other");
            }
            (Some(key), None) => {
                validate_master_key(key, "master_key")?;
            }
            (None, Some(kms)) => {
                kms.validate()?;
            }
            (None, None) => {
                bail!("[encryption] either master_key or [encryption.kms] is required when the [encryption] section is present");
            }
        }
        if let Some(ref key) = self.previous_master_key {
            validate_master_key(key, "previous_master_key")?;
        }
        Ok(())
    }
}

/// Validates that a base64-encoded key decodes to exactly 32 bytes.
fn validate_master_key(key: &str, field_name: &str) -> Result<()> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(key)
        .with_context(|| format!("[encryption] {field_name} is not valid base64"))?;
    if bytes.len() != 32 {
        bail!(
            "[encryption] {field_name} must decode to exactly 32 bytes, got {}",
            bytes.len()
        );
    }
    Ok(())
}

/// Monitoring and audit configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct MonitoringConfig {
    /// Audit log configuration.
    pub audit: Option<AuditConfig>,
    /// Metrics snapshot configuration.
    pub metrics: Option<MetricsConfig>,
}

/// Audit log configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct AuditConfig {
    /// Whether audit logging is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Number of days to retain audit log entries. 0 = keep forever.
    /// When set, this value is locked (read-only in console).
    /// When absent, the retention can be set via the Admin API / console.
    pub retention_days: Option<u32>,
}

/// Metrics snapshot configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct MetricsConfig {
    /// Whether metrics collection is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Number of days to retain metrics snapshots. 0 = keep forever.
    /// When set, this value is locked (read-only in console).
    /// When absent, the retention can be set via the Admin API / console.
    pub retention_days: Option<u32>,
    /// How often to snapshot gauge metrics, in seconds.
    #[serde(default = "default_metrics_interval")]
    pub interval_seconds: u64,
}

/// Notifications configuration (optional tuning parameters).
///
/// Notifications are always enabled. This section is only needed to override
/// operational defaults (channel size, retry behavior, webhook timeout, retention).
#[derive(Debug, Clone, Deserialize)]
pub struct NotificationsConfig {
    /// In-memory event channel buffer size.
    #[serde(default = "default_channel_size")]
    pub channel_size: usize,
    /// Maximum webhook delivery retries.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Base retry backoff in seconds (exponential: base * 2^attempt).
    #[serde(default = "default_retry_base_seconds")]
    pub retry_base_seconds: u64,
    /// Webhook HTTP POST timeout in seconds.
    #[serde(default = "default_webhook_timeout_seconds")]
    pub webhook_timeout_seconds: u64,
    /// Redis connector connection timeout in seconds.
    #[serde(default = "default_redis_timeout_seconds")]
    pub redis_timeout_seconds: u64,
    /// NATS connector connection timeout in seconds.
    #[serde(default = "default_nats_timeout_seconds")]
    pub nats_timeout_seconds: u64,
    /// MQTT connector connection timeout in seconds.
    #[serde(default = "default_mqtt_timeout_seconds")]
    pub mqtt_timeout_seconds: u64,
    /// PostgreSQL connector connection timeout in seconds.
    #[serde(default = "default_postgresql_timeout_seconds")]
    pub postgresql_timeout_seconds: u64,
    /// MySQL connector connection timeout in seconds.
    #[serde(default = "default_mysql_timeout_seconds")]
    pub mysql_timeout_seconds: u64,
    /// MongoDB connector connection timeout in seconds.
    #[serde(default = "default_mongodb_timeout_seconds")]
    pub mongodb_timeout_seconds: u64,
    /// Kafka connector connection timeout in seconds.
    #[serde(default = "default_kafka_timeout_seconds")]
    pub kafka_timeout_seconds: u64,
    /// AMQP connector connection timeout in seconds.
    #[serde(default = "default_amqp_timeout_seconds")]
    pub amqp_timeout_seconds: u64,
    /// Elasticsearch connector connection timeout in seconds.
    #[serde(default = "default_elasticsearch_timeout_seconds")]
    pub elasticsearch_timeout_seconds: u64,
    /// Syslog connector connection timeout in seconds.
    #[serde(default = "default_syslog_timeout_seconds")]
    pub syslog_timeout_seconds: u64,
    /// SMTP connector operation timeout in seconds.
    #[serde(default = "default_smtp_timeout_seconds")]
    pub smtp_timeout_seconds: u64,
    /// gRPC connector operation timeout in seconds.
    #[serde(default = "default_grpc_timeout_seconds")]
    pub grpc_timeout_seconds: u64,
    /// Number of days to retain notification events. 0 = keep forever.
    /// When set in TOML, locked (read-only in console). When absent, console can set it.
    pub event_retention_days: Option<u32>,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            channel_size: default_channel_size(),
            max_retries: default_max_retries(),
            retry_base_seconds: default_retry_base_seconds(),
            webhook_timeout_seconds: default_webhook_timeout_seconds(),
            redis_timeout_seconds: default_redis_timeout_seconds(),
            nats_timeout_seconds: default_nats_timeout_seconds(),
            mqtt_timeout_seconds: default_mqtt_timeout_seconds(),
            postgresql_timeout_seconds: default_postgresql_timeout_seconds(),
            mysql_timeout_seconds: default_mysql_timeout_seconds(),
            mongodb_timeout_seconds: default_mongodb_timeout_seconds(),
            kafka_timeout_seconds: default_kafka_timeout_seconds(),
            amqp_timeout_seconds: default_amqp_timeout_seconds(),
            elasticsearch_timeout_seconds: default_elasticsearch_timeout_seconds(),
            syslog_timeout_seconds: default_syslog_timeout_seconds(),
            smtp_timeout_seconds: default_smtp_timeout_seconds(),
            grpc_timeout_seconds: default_grpc_timeout_seconds(),
            event_retention_days: None,
        }
    }
}

/// Lifecycle evaluation worker configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct LifecycleWorkerConfig {
    /// How often the worker evaluates lifecycle rules, in seconds.
    /// The interval is fixed at startup.
    #[serde(default = "default_lifecycle_interval")]
    pub interval_seconds: u64,
}

impl Default for LifecycleWorkerConfig {
    fn default() -> Self {
        Self {
            interval_seconds: default_lifecycle_interval(),
        }
    }
}

impl LifecycleWorkerConfig {
    pub fn validate(&self) -> Result<()> {
        if self.interval_seconds == 0 {
            bail!("[lifecycle] interval_seconds must be at least 1");
        }
        Ok(())
    }
}

fn default_lifecycle_interval() -> u64 {
    3600
}

/// Replication worker configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct ReplicationConfig {
    /// How often the worker polls the journal, in seconds.
    #[serde(default = "default_poll_interval")]
    pub poll_interval_seconds: u64,
    /// Maximum number of journal entries processed per tick.
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    /// Maximum delivery attempts before marking an entry `failed`.
    #[serde(default = "default_replication_max_retries")]
    pub max_retries: u32,
    /// Base retry backoff in seconds (exponential: base * 2^attempt, capped at 1h).
    #[serde(default = "default_replication_retry_base")]
    pub retry_base_seconds: u64,
    /// Outbound HTTP request timeout in seconds.
    #[serde(default = "default_replication_request_timeout")]
    pub request_timeout_seconds: u64,
    /// Stable identifier for this Arca instance. Sent as the
    /// `x-amz-arca-replication-source` header on every outbound request so
    /// the receiver can flag the object REPLICA and skip its own journal emit
    /// (loop prevention for mirror configurations).
    #[serde(default = "default_source_endpoint_id")]
    pub source_endpoint_id: String,
    /// Days after which COMPLETED journal rows are purged by the retention
    /// worker. Keeps the journal bounded during normal operation.
    #[serde(default = "default_journal_retention_days")]
    pub journal_retention_days: u32,
    /// Hard cap in days for ANY journal row regardless of status. Prevents
    /// unbounded growth when a destination stays offline forever.
    #[serde(default = "default_journal_max_age_days")]
    pub journal_max_age_days: u32,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            poll_interval_seconds: default_poll_interval(),
            batch_size: default_batch_size(),
            max_retries: default_replication_max_retries(),
            retry_base_seconds: default_replication_retry_base(),
            request_timeout_seconds: default_replication_request_timeout(),
            source_endpoint_id: default_source_endpoint_id(),
            journal_retention_days: default_journal_retention_days(),
            journal_max_age_days: default_journal_max_age_days(),
        }
    }
}

fn default_poll_interval() -> u64 {
    15
}
fn default_batch_size() -> u32 {
    100
}
fn default_replication_max_retries() -> u32 {
    10
}
fn default_replication_retry_base() -> u64 {
    5
}
fn default_replication_request_timeout() -> u64 {
    60
}
fn default_source_endpoint_id() -> String {
    // Fall back to a stable per-host value. Operators should set this
    // explicitly; the default is merely "non-empty" so loop prevention works
    // out of the box in single-instance setups.
    "arca".to_string()
}
fn default_journal_retention_days() -> u32 {
    30
}
fn default_journal_max_age_days() -> u32 {
    90
}

/// Consistency policy for cluster writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClusterMode {
    /// CP: a write is acknowledged only once a majority of nodes hold it.
    /// A node in the minority becomes read-only. No divergence possible.
    #[default]
    Quorum,
    /// AP: always writable (even a single node); conflicts on partition heal
    /// resolve last-write-wins.
    Available,
}

/// How peers discover each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DiscoveryMode {
    /// mDNS on the local subnet (no daemon). Default.
    #[default]
    Mdns,
    /// Static seed list (identical on every node; a node ignores itself).
    Static,
    /// A DNS name resolving to all peers (e.g. a Kubernetes headless Service).
    Dns,
}

/// Cluster (High Availability) configuration.
///
/// This section is **identical on every node**: there is no per-node `node_id`
/// or peer list. Each node derives and persists its own `node_id`, and peers
/// are discovered automatically. See `cluster/` for the membership manager.
#[derive(Debug, Clone, Deserialize)]
pub struct ClusterConfig {
    /// Whether clustering is enabled.
    #[serde(default)]
    pub enabled: bool,
    /// Logical cluster name. Only nodes sharing the same `cluster_id` form a
    /// cluster (used as the discovery filter).
    pub cluster_id: String,
    /// Shared secret authenticating inter-node `/cluster/v1/*` requests
    /// (identical on every node). Must be a high-entropy value of at least 16
    /// characters (M5/§3.7(B)); the shipped placeholders are refused.
    pub secret: String,
    /// Previous shared secret, accepted INBOUND only, for zero-downtime
    /// rotation (H8/D3b). Outbound requests, the ping challenge MAC and the
    /// config fingerprint always use `secret`. Runbook: set `secret_previous`
    /// to the old value and `secret` to the new one on every node, rolling
    /// restart, then remove `secret_previous`.
    pub secret_previous: Option<String>,
    /// Consistency policy: "quorum" (CP, default) or "available" (AP).
    #[serde(default)]
    pub mode: ClusterMode,
    /// Expected cluster size. Required in `mode = "quorum"` to derive the
    /// write majority; ignored in `mode = "available"`.
    pub cluster_size: Option<u32>,
    /// Discovery mechanism (default: mdns).
    #[serde(default)]
    pub discovery: DiscoveryMode,
    /// Port peers use to reach this node. Optional; defaults to `[server].port`.
    /// Set only when it differs from the bind port (container port mapping/NAT).
    pub advertise_port: Option<u16>,
    /// Host/IP advertised to peers. Optional; defaults to the auto-detected
    /// interface address (useful when bind = 0.0.0.0).
    pub advertise_addr: Option<String>,
    /// Seed endpoints for `discovery = "static"` (identical on every node;
    /// a node ignores its own entry).
    #[serde(default)]
    pub seeds: Vec<String>,
    /// DNS name for `discovery = "dns"` (resolves to all peers).
    pub dns_name: Option<String>,
    /// Interval between peer health pings, in seconds.
    #[serde(default = "default_cluster_health_interval")]
    pub health_interval_seconds: u64,
    /// Interval between anti-entropy reconciliation passes, in seconds.
    #[serde(default = "default_cluster_anti_entropy_interval")]
    pub anti_entropy_interval_seconds: u64,
    /// Inter-node HTTP request timeout, in seconds.
    #[serde(default = "default_cluster_request_timeout")]
    pub request_timeout_seconds: u64,
    /// How long a tombstone (hard-deleted version kept for cluster convergence)
    /// is retained before garbage collection, in days. MUST exceed the longest
    /// expected node downtime: a node that returns after the grace window would
    /// no longer receive the tombstone and could resurrect the deleted object.
    #[serde(default = "default_cluster_tombstone_grace_days")]
    pub tombstone_grace_days: u64,
    /// Advanced override of `tombstone_grace_days` with seconds granularity.
    /// Exists for integration tests and demos that must observe the GC
    /// liveness guard (§3.2) within seconds — production deployments should
    /// size the grace in days. When set it wins over `tombstone_grace_days`.
    pub tombstone_grace_seconds: Option<u64>,
    /// How long an unreachable peer stays in membership before it is pruned
    /// (M3), in days. Defaults to `tombstone_grace_days`: while a dead peer is
    /// still remembered it blocks tombstone GC (a purge it missed could
    /// resurrect deletions on its return — review §3.2); once pruned it stops
    /// blocking, and a later return must be treated as a re-sync.
    pub peer_prune_days: Option<u64>,
    /// Review M2: maximum blob repairs (peer fetches) attempted per
    /// anti-entropy tick by the proactive blob-repair sweep (default 100).
    /// A sweep that exhausts the budget resumes where it left off on the NEXT
    /// tick, so one huge repair backlog cannot monopolize the worker for hours
    /// while still draining at `budget / anti_entropy_interval` per node.
    pub blob_repair_budget: Option<u32>,
    /// Inter-node mutual TLS (R4/H12 — resolves TD-015). REQUIRED when the
    /// cluster runs over HTTPS (`[server.tls]` enabled): there is no insecure
    /// fallback. Meaningless (and rejected) without `[server.tls]`.
    pub tls: Option<ClusterTlsConfig>,
}

/// Inter-node mutual-TLS material (`[cluster.tls]`, decision H12).
///
/// The CA is operator-distributed: every node gets the same `ca_file` plus its
/// own cert/key signed by that CA (`arca tls generate-cluster` mints the whole
/// set). Outbound inter-node clients verify peer server certificates against
/// the CA (on top of the system roots) and present `cert_file` as their client
/// identity; the listener requests client certificates, and the
/// `/cluster/v1/*` routes refuse requests that did not present one signed by
/// the CA. Paths are used as given (absolute paths recommended).
#[derive(Debug, Clone, Deserialize)]
pub struct ClusterTlsConfig {
    /// Cluster CA certificate (PEM): the trust anchor for both directions.
    pub ca_file: String,
    /// This node's certificate (PEM), signed by the cluster CA.
    pub cert_file: String,
    /// This node's private key (PEM).
    pub key_file: String,
}

impl ClusterTlsConfig {
    pub fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("ca_file", &self.ca_file),
            ("cert_file", &self.cert_file),
            ("key_file", &self.key_file),
        ] {
            if value.trim().is_empty() {
                bail!("[cluster.tls] {name} is required and must be non-empty");
            }
        }
        Ok(())
    }
}

/// Shipped placeholder secrets (M5/§3.7(B)): they appear verbatim in the
/// reference configs and deploy manifests, so they are the first guess of any
/// attacker. Startup refuses them outright.
const PLACEHOLDER_CLUSTER_SECRETS: &[&str] =
    &["dev-cluster-secret-change-me", "CHANGEME-CLUSTER-SECRET"];

/// M5/§3.7(B): the cluster secret keys ALL inter-node authentication (the
/// SigV4 signatures and the peer challenge MAC), so a guessable value hands an
/// attacker the whole cluster. Hard floor enforced here; the softer
/// "looks low-entropy" heuristic is [`ClusterConfig::secret_looks_low_entropy`].
fn validate_cluster_secret(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("[cluster] {field} is required and must be non-empty");
    }
    if PLACEHOLDER_CLUSTER_SECRETS.contains(&value) {
        bail!(
            "[cluster] {field} is the shipped placeholder — set a real secret \
             (e.g. `openssl rand -hex 32`), identical on every node"
        );
    }
    if value.len() < 16 {
        bail!(
            "[cluster] {field} is too short ({} chars): use at least 16, ideally a \
             high-entropy random value (e.g. `openssl rand -hex 32`)",
            value.len()
        );
    }
    Ok(())
}

impl ClusterConfig {
    /// Validates the cluster configuration (only meaningful when `enabled`).
    pub fn validate(&self) -> Result<()> {
        if self.cluster_id.trim().is_empty() {
            bail!("[cluster] cluster_id is required and must be non-empty");
        }
        validate_cluster_secret(&self.secret, "secret")?;
        if let Some(prev) = &self.secret_previous {
            validate_cluster_secret(prev, "secret_previous")?;
            if prev == &self.secret {
                bail!(
                    "[cluster] secret_previous equals secret — remove it \
                     (it only exists to bridge a rotation)"
                );
            }
        }
        if self.mode == ClusterMode::Quorum {
            match self.cluster_size {
                None => bail!("[cluster] cluster_size is required when mode = \"quorum\""),
                Some(n) if n < 1 => bail!("[cluster] cluster_size must be >= 1"),
                _ => {}
            }
        }
        match self.discovery {
            DiscoveryMode::Static => {
                if self.seeds.is_empty() {
                    bail!("[cluster] seeds is required and must be non-empty when discovery = \"static\"");
                }
            }
            DiscoveryMode::Dns => {
                if self.dns_name.as_ref().map_or(true, |s| s.trim().is_empty()) {
                    bail!("[cluster] dns_name is required when discovery = \"dns\"");
                }
            }
            DiscoveryMode::Mdns => {}
        }
        if self.peer_prune_days == Some(0) {
            bail!("[cluster] peer_prune_days must be >= 1 when set");
        }
        if self.tombstone_grace_seconds == Some(0) {
            bail!("[cluster] tombstone_grace_seconds must be >= 1 when set");
        }
        if self.blob_repair_budget == Some(0) {
            bail!("[cluster] blob_repair_budget must be >= 1 when set");
        }
        if let Some(tls) = &self.tls {
            tls.validate()?;
        }
        Ok(())
    }

    /// M5 heuristic for the "secret looks low-entropy" warning: fewer than 8
    /// distinct characters, or a single character class (only lowercase, only
    /// digits, ...). The caller logs the warning — config loads before tracing
    /// is initialized in `serve`, so it cannot be emitted here.
    pub fn secret_looks_low_entropy(&self) -> bool {
        let s = &self.secret;
        let distinct = s.chars().collect::<std::collections::BTreeSet<_>>().len();
        let classes = [
            s.chars().any(|c| c.is_ascii_lowercase()),
            s.chars().any(|c| c.is_ascii_uppercase()),
            s.chars().any(|c| c.is_ascii_digit()),
            s.chars().any(|c| !c.is_ascii_alphanumeric()),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        distinct < 8 || classes < 2
    }

    /// Number of durable copies (including the local node) required to
    /// acknowledge a write under `mode = "quorum"`: the majority
    /// `floor(cluster_size/2)+1`. Returns `None` in `mode = "available"`,
    /// where any single node may acknowledge.
    pub fn write_quorum(&self) -> Option<u32> {
        match self.mode {
            ClusterMode::Quorum => self.cluster_size.map(|n| n / 2 + 1),
            ClusterMode::Available => None,
        }
    }

    /// Tombstone retention as a `Duration`: the seconds-granularity override
    /// when set (tests/demos), otherwise `tombstone_grace_days`.
    pub fn tombstone_grace(&self) -> std::time::Duration {
        match self.tombstone_grace_seconds {
            Some(s) => std::time::Duration::from_secs(s),
            None => std::time::Duration::from_secs(self.tombstone_grace_days * 24 * 60 * 60),
        }
    }

    /// Effective membership prune window in days (M3): the configured
    /// `peer_prune_days`, defaulting to `tombstone_grace_days` so a dead peer
    /// stops blocking tombstone GC exactly when keeping its tombstones can no
    /// longer help it.
    pub fn peer_prune_days(&self) -> u64 {
        self.peer_prune_days.unwrap_or(self.tombstone_grace_days)
    }

    /// Effective per-tick blob-repair budget (M2; see
    /// [`ClusterConfig::blob_repair_budget`]).
    pub fn blob_repair_budget(&self) -> u32 {
        self.blob_repair_budget.unwrap_or(100)
    }
}

fn default_cluster_health_interval() -> u64 {
    5
}
fn default_cluster_anti_entropy_interval() -> u64 {
    30
}
fn default_cluster_request_timeout() -> u64 {
    10
}
fn default_cluster_tombstone_grace_days() -> u64 {
    7
}

fn default_channel_size() -> usize {
    10_000
}
fn default_max_retries() -> u32 {
    3
}
fn default_retry_base_seconds() -> u64 {
    1
}
fn default_webhook_timeout_seconds() -> u64 {
    30
}
fn default_redis_timeout_seconds() -> u64 {
    5
}
fn default_nats_timeout_seconds() -> u64 {
    5
}
fn default_mqtt_timeout_seconds() -> u64 {
    5
}
fn default_postgresql_timeout_seconds() -> u64 {
    5
}
fn default_mysql_timeout_seconds() -> u64 {
    5
}
fn default_mongodb_timeout_seconds() -> u64 {
    5
}
fn default_kafka_timeout_seconds() -> u64 {
    10
}
fn default_amqp_timeout_seconds() -> u64 {
    5
}
fn default_elasticsearch_timeout_seconds() -> u64 {
    5
}
fn default_syslog_timeout_seconds() -> u64 {
    5
}
fn default_smtp_timeout_seconds() -> u64 {
    15
}
fn default_grpc_timeout_seconds() -> u64 {
    10
}
fn default_true() -> bool {
    true
}

fn default_metrics_interval() -> u64 {
    60
}

/// Applies environment variable overrides to the loaded configuration.
///
/// Supported variables:
/// - `ARCA_SERVER_BIND` — overrides `server.bind`
/// - `ARCA_SERVER_PORT` — overrides `server.port`
/// - `ARCA_STORAGE_DATA_DIR` — overrides `storage.data_dir`
fn apply_env_overrides(config: &mut Config) -> Result<()> {
    if let Ok(val) = std::env::var("ARCA_SERVER_BIND") {
        tracing::info!(bind = %val, "Overriding server.bind from ARCA_SERVER_BIND");
        config.server.bind = val;
    }
    if let Ok(val) = std::env::var("ARCA_SERVER_PORT") {
        let port: u16 = val
            .parse()
            .with_context(|| format!("ARCA_SERVER_PORT: invalid port number \"{val}\""))?;
        tracing::info!(port, "Overriding server.port from ARCA_SERVER_PORT");
        config.server.port = port;
    }
    if let Ok(val) = std::env::var("ARCA_STORAGE_DATA_DIR") {
        tracing::info!(data_dir = %val, "Overriding storage.data_dir from ARCA_STORAGE_DATA_DIR");
        config.storage.data_dir = val;
    }
    Ok(())
}

/// Loads configuration from a TOML file.
pub fn load_config(path: &Path) -> Result<Config> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading config: {}", path.display()))?;
    let mut config: Config =
        toml::from_str(&content).with_context(|| format!("parsing config: {}", path.display()))?;
    apply_env_overrides(&mut config)?;
    // Auto-detect postgres backend when [storage.postgres] section is present
    // but metadata_backend was not explicitly set (still at default "sqlite").
    // This avoids TOML fragment concatenation issues where the bare key would
    // land in the wrong section.
    if config.storage.metadata_backend == "sqlite" && config.storage.postgres.is_some() {
        config.storage.metadata_backend = "postgres".to_string();
    }
    if let Some(tls) = &config.server.tls {
        tls.validate()?;
    }
    if let Some(enc) = &config.encryption {
        enc.validate()?;
    }
    if let Some(lifecycle) = &config.lifecycle {
        lifecycle.validate()?;
    }
    if let Some(cluster) = &config.cluster {
        if cluster.enabled {
            cluster.validate()?;
            validate_cluster_transport(config.server.tls.as_ref(), cluster)?;
        }
    }
    config.storage.validate()?;
    Ok(config)
}

/// R4 (decision H12, resolves TD-015): inter-node TLS is VERIFIED — there is
/// no insecure fallback. A cluster over HTTPS therefore requires the
/// `[cluster.tls]` material (fail closed, confirmed 2026-06-11); without
/// `[server.tls]` that material is meaningless. The existing `[server.tls]
/// ca_file` (global, REQUIRED client certs for every connection) and the
/// cluster CA (client certs optional at the TLS layer, enforced on
/// `/cluster/v1/*` only) need conflicting listener policies, so the
/// combination is rejected until someone actually needs it.
fn validate_cluster_transport(server_tls: Option<&TlsConfig>, cluster: &ClusterConfig) -> Result<()> {
    match (server_tls, &cluster.tls) {
        (Some(_), None) => bail!(
            "[cluster] over HTTPS requires [cluster.tls]: inter-node clients verify \
             peer certificates against a shared cluster CA (TD-015 — no insecure \
             fallback). Generate the material with `arca tls generate-cluster` and \
             set ca_file/cert_file/key_file on every node"
        ),
        (None, Some(_)) => bail!(
            "[cluster.tls] requires [server.tls]: cluster certificates ride the \
             same listener as the S3 API"
        ),
        (Some(tls), Some(_)) if tls.ca_file.is_some() => bail!(
            "[server.tls] ca_file (global client mTLS) and [cluster.tls] are \
             mutually exclusive: the listener can enforce only one \
             client-certificate policy"
        ),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_toml() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.server.port, 9000);
        assert_eq!(config.storage.data_dir, "/data");
    }

    #[test]
    fn parse_missing_field_fails() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
"#;
        let result: Result<Config, _> = toml::from_str(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn db_path_derivation() {
        let storage = StorageConfig {
            data_dir: "/data".to_string(),
            blob_prefix_depth: 2,
            metadata_backend: "sqlite".to_string(),
            postgres: None,
            blob_gc_enabled: false,
            blob_gc_interval_seconds: 3600,
            blob_gc_grace_seconds: 86400,
        };
        assert_eq!(
            storage.db_path(),
            std::path::PathBuf::from("/data/arca.db")
        );
    }

    #[test]
    fn blobs_dir_derivation() {
        let storage = StorageConfig {
            data_dir: "/data".to_string(),
            blob_prefix_depth: 2,
            metadata_backend: "sqlite".to_string(),
            postgres: None,
            blob_gc_enabled: false,
            blob_gc_interval_seconds: 3600,
            blob_gc_grace_seconds: 86400,
        };
        assert_eq!(
            storage.blobs_dir(),
            std::path::PathBuf::from("/data/blobs")
        );
    }

    #[test]
    fn blob_prefix_depth_default() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.storage.blob_prefix_depth, 2);
    }

    #[test]
    fn blob_gc_defaults_off() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.storage.blob_gc_enabled);
        assert_eq!(config.storage.blob_gc_interval_seconds, 3600);
        assert_eq!(config.storage.blob_gc_grace_seconds, 86400);
    }

    #[test]
    fn blob_gc_parsed_and_interval_never_zero() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
blob_gc_enabled = true
blob_gc_interval_seconds = 0
blob_gc_grace_seconds = 120
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.storage.blob_gc_enabled);
        // The accessor clamps to >= 1s so tokio's interval timer never panics.
        assert_eq!(config.storage.blob_gc_interval(), std::time::Duration::from_secs(1));
        assert_eq!(config.storage.blob_gc_grace(), std::time::Duration::from_secs(120));
    }

    #[test]
    fn parse_config_without_tls() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.server.tls.is_none());
    }

    #[test]
    fn parse_config_tls_cert_dir_only() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.tls]
cert_dir = "/etc/arca/certs"

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let tls = config.server.tls.unwrap();
        assert_eq!(tls.cert_dir.as_deref(), Some("/etc/arca/certs"));
        assert!(tls.cert_file.is_none());
        assert!(tls.key_file.is_none());
    }

    #[test]
    fn parse_config_tls_dir_and_files() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.tls]
cert_dir = "/etc/arca/certs"
cert_file = "server.crt"
key_file = "server.key"

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let tls = config.server.tls.unwrap();
        assert_eq!(tls.cert_dir.as_deref(), Some("/etc/arca/certs"));
        assert_eq!(tls.cert_file.as_deref(), Some("server.crt"));
        assert_eq!(tls.key_file.as_deref(), Some("server.key"));
    }

    #[test]
    fn parse_config_tls_absolute_paths() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.tls]
cert_file = "/etc/ssl/certs/arca.pem"
key_file = "/etc/ssl/private/arca.key"

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let tls = config.server.tls.unwrap();
        assert!(tls.cert_dir.is_none());
        assert_eq!(tls.cert_file.as_deref(), Some("/etc/ssl/certs/arca.pem"));
        assert_eq!(tls.key_file.as_deref(), Some("/etc/ssl/private/arca.key"));
    }

    #[test]
    fn validate_no_cert_dir_no_files_fails() {
        let tls = TlsConfig {
            cert_dir: None,
            cert_file: None,
            key_file: None,
            ca_file: None,
        };
        assert!(tls.validate().is_err());
    }

    #[test]
    fn parse_config_with_encryption() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[encryption]
enabled = true
master_key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let enc = config.encryption.unwrap();
        assert!(enc.enabled);
        assert!(enc.master_key.is_some());
    }

    #[test]
    fn parse_config_without_encryption() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.encryption.is_none());
    }

    #[test]
    fn encryption_section_requires_master_key_or_kms() {
        // Neither master_key nor kms → error
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
            kms: None,
        };
        assert!(enc.validate().is_err());

        let enc = EncryptionConfig {
            enabled: false,
            master_key: None,
            previous_master_key: None,
            kms: None,
        };
        assert!(enc.validate().is_err());
    }

    #[test]
    fn encryption_master_key_must_be_valid_base64() {
        let enc = EncryptionConfig {
            enabled: true,
            master_key: Some("not-valid-base64!!!".to_string()),
            previous_master_key: None,
            kms: None,
        };
        assert!(enc.validate().is_err());
    }

    #[test]
    fn encryption_master_key_must_be_32_bytes() {
        // 16 bytes, not 32
        let enc = EncryptionConfig {
            enabled: true,
            master_key: Some("AAAAAAAAAAAAAAAAAAAAAA==".to_string()),
            previous_master_key: None,
            kms: None,
        };
        assert!(enc.validate().is_err());
    }

    #[test]
    fn encryption_valid_config() {
        // 32 bytes in base64 = 44 chars (with padding)
        let enc = EncryptionConfig {
            enabled: true,
            master_key: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()),
            previous_master_key: None,
            kms: None,
        };
        assert!(enc.validate().is_ok());
    }

    #[test]
    fn encryption_master_key_and_kms_mutually_exclusive() {
        let enc = EncryptionConfig {
            enabled: true,
            master_key: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_string()),
            previous_master_key: None,
            kms: Some(KmsConfig {
                endpoint: "http://vault:8200".to_string(),
                secret_path: default_secret_path(),
                secret_field: default_secret_field(),
                auth_method: KmsAuthMethod::Token,
                token: Some("dev-root-token".to_string()),
                role_id: None,
                secret_id: None,
                tls_skip_verify: false,
                ca_file: None,
            }),
        };
        let err = enc.validate().unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "got: {err}");
    }

    #[test]
    fn encryption_kms_token_auth_valid() {
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
            kms: Some(KmsConfig {
                endpoint: "http://vault:8200".to_string(),
                secret_path: default_secret_path(),
                secret_field: default_secret_field(),
                auth_method: KmsAuthMethod::Token,
                token: Some("dev-root-token".to_string()),
                role_id: None,
                secret_id: None,
                tls_skip_verify: false,
                ca_file: None,
            }),
        };
        assert!(enc.validate().is_ok());
    }

    #[test]
    fn encryption_kms_approle_auth_valid() {
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
            kms: Some(KmsConfig {
                endpoint: "http://vault:8200".to_string(),
                secret_path: default_secret_path(),
                secret_field: default_secret_field(),
                auth_method: KmsAuthMethod::Approle,
                token: None,
                role_id: Some("my-role-id".to_string()),
                secret_id: Some("my-secret-id".to_string()),
                tls_skip_verify: false,
                ca_file: None,
            }),
        };
        assert!(enc.validate().is_ok());
    }

    #[test]
    fn encryption_kms_token_auth_missing_token() {
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
            kms: Some(KmsConfig {
                endpoint: "http://vault:8200".to_string(),
                secret_path: default_secret_path(),
                secret_field: default_secret_field(),
                auth_method: KmsAuthMethod::Token,
                token: None,
                role_id: None,
                secret_id: None,
                tls_skip_verify: false,
                ca_file: None,
            }),
        };
        let err = enc.validate().unwrap_err().to_string();
        assert!(err.contains("token is required"), "got: {err}");
    }

    #[test]
    fn encryption_kms_approle_missing_credentials() {
        // Missing role_id
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
            kms: Some(KmsConfig {
                endpoint: "http://vault:8200".to_string(),
                secret_path: default_secret_path(),
                secret_field: default_secret_field(),
                auth_method: KmsAuthMethod::Approle,
                token: None,
                role_id: None,
                secret_id: Some("secret".to_string()),
                tls_skip_verify: false,
                ca_file: None,
            }),
        };
        let err = enc.validate().unwrap_err().to_string();
        assert!(err.contains("role_id is required"), "got: {err}");

        // Missing secret_id
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
            kms: Some(KmsConfig {
                endpoint: "http://vault:8200".to_string(),
                secret_path: default_secret_path(),
                secret_field: default_secret_field(),
                auth_method: KmsAuthMethod::Approle,
                token: None,
                role_id: Some("role".to_string()),
                secret_id: None,
                tls_skip_verify: false,
                ca_file: None,
            }),
        };
        let err = enc.validate().unwrap_err().to_string();
        assert!(err.contains("secret_id is required"), "got: {err}");
    }

    #[test]
    fn encryption_kms_empty_endpoint() {
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
            kms: Some(KmsConfig {
                endpoint: "".to_string(),
                secret_path: default_secret_path(),
                secret_field: default_secret_field(),
                auth_method: KmsAuthMethod::Token,
                token: Some("tok".to_string()),
                role_id: None,
                secret_id: None,
                tls_skip_verify: false,
                ca_file: None,
            }),
        };
        let err = enc.validate().unwrap_err().to_string();
        assert!(err.contains("endpoint is required"), "got: {err}");
    }

    #[test]
    fn parse_config_with_kms_token() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[encryption]
enabled = true

[encryption.kms]
endpoint = "http://vault:8200"
auth_method = "token"
token = "dev-root-token"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let enc = config.encryption.unwrap();
        assert!(enc.enabled);
        assert!(enc.master_key.is_none());
        let kms = enc.kms.unwrap();
        assert_eq!(kms.endpoint, "http://vault:8200");
        assert_eq!(kms.secret_path, "secret/arca/master-key");
        assert_eq!(kms.secret_field, "key");
        assert!(matches!(kms.auth_method, KmsAuthMethod::Token));
        assert_eq!(kms.token.as_deref(), Some("dev-root-token"));
    }

    #[test]
    fn parse_config_with_kms_approle() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[encryption]
enabled = true

[encryption.kms]
endpoint = "https://vault.prod:8200"
secret_path = "kv/myapp/encryption-key"
secret_field = "master_key"
auth_method = "approle"
role_id = "abc-123"
secret_id = "def-456"
ca_file = "/etc/ssl/vault-ca.pem"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let enc = config.encryption.unwrap();
        let kms = enc.kms.unwrap();
        assert_eq!(kms.endpoint, "https://vault.prod:8200");
        assert_eq!(kms.secret_path, "kv/myapp/encryption-key");
        assert_eq!(kms.secret_field, "master_key");
        assert!(matches!(kms.auth_method, KmsAuthMethod::Approle));
        assert_eq!(kms.role_id.as_deref(), Some("abc-123"));
        assert_eq!(kms.secret_id.as_deref(), Some("def-456"));
        assert_eq!(kms.ca_file.as_deref(), Some("/etc/ssl/vault-ca.pem"));
    }

    #[test]
    fn parse_config_with_region() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000
region = "eu-west-1"

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.server.region.as_deref(), Some("eu-west-1"));
    }

    #[test]
    fn parse_config_without_region() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.server.region.is_none());
    }

    #[test]
    fn parse_config_with_monitoring() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[monitoring.audit]
enabled = true
retention_days = 90

[monitoring.metrics]
enabled = true
retention_days = 30
interval_seconds = 120
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let mon = config.monitoring.unwrap();
        let audit = mon.audit.unwrap();
        assert!(audit.enabled);
        assert_eq!(audit.retention_days, Some(90));
        let metrics = mon.metrics.unwrap();
        assert!(metrics.enabled);
        assert_eq!(metrics.retention_days, Some(30));
        assert_eq!(metrics.interval_seconds, 120);
    }

    #[test]
    fn parse_config_monitoring_defaults() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[monitoring.audit]

[monitoring.metrics]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let mon = config.monitoring.unwrap();
        let audit = mon.audit.unwrap();
        assert!(audit.enabled); // default true
        assert!(audit.retention_days.is_none()); // not set, console can manage
        let metrics = mon.metrics.unwrap();
        assert!(metrics.enabled); // default true
        assert!(metrics.retention_days.is_none()); // not set, console can manage
        assert_eq!(metrics.interval_seconds, 60); // default
    }

    #[test]
    fn parse_config_without_monitoring() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.monitoring.is_none());
    }

    #[test]
    fn validate_cert_dir_plus_only_one_file_fails() {
        let tls = TlsConfig {
            cert_dir: Some("/certs".into()),
            cert_file: Some("server.crt".into()),
            key_file: None,
            ca_file: None,
        };
        assert!(tls.validate().is_err());

        let tls = TlsConfig {
            cert_dir: Some("/certs".into()),
            cert_file: None,
            key_file: Some("server.key".into()),
            ca_file: None,
        };
        assert!(tls.validate().is_err());
    }

    /// Helper: build a minimal Config for env override tests.
    fn base_config() -> Config {
        Config {
            server: ServerConfig {
                bind: "0.0.0.0".to_string(),
                port: 9000,
                domain: None,
                region: None,
                log_level: None,
                tls: None,
                limits: None,
                cache: None,
                runtime: None,
                http: None,
            },
            storage: StorageConfig {
                data_dir: "/data".to_string(),
                blob_prefix_depth: 2,
                metadata_backend: "sqlite".to_string(),
                postgres: None,
                blob_gc_enabled: false,
                blob_gc_interval_seconds: 3600,
                blob_gc_grace_seconds: 86400,
            },
            encryption: None,
            monitoring: None,
            notifications: None,
            replication: None,
            lifecycle: None,
            cluster: None,
        }
    }

    #[test]
    fn env_override_server_bind() {
        // Use a unique env var name via temp_env pattern: set, run, unset.
        std::env::set_var("ARCA_SERVER_BIND", "127.0.0.1");
        let mut config = base_config();
        apply_env_overrides(&mut config).unwrap();
        std::env::remove_var("ARCA_SERVER_BIND");

        assert_eq!(config.server.bind, "127.0.0.1");
    }

    #[test]
    fn env_override_server_port() {
        std::env::set_var("ARCA_SERVER_PORT", "8080");
        let mut config = base_config();
        apply_env_overrides(&mut config).unwrap();
        std::env::remove_var("ARCA_SERVER_PORT");

        assert_eq!(config.server.port, 8080);
    }

    #[test]
    fn env_override_storage_data_dir() {
        std::env::set_var("ARCA_STORAGE_DATA_DIR", "/mnt/storage");
        let mut config = base_config();
        apply_env_overrides(&mut config).unwrap();
        std::env::remove_var("ARCA_STORAGE_DATA_DIR");

        assert_eq!(config.storage.data_dir, "/mnt/storage");
    }

    #[test]
    fn env_override_invalid_port_fails() {
        std::env::set_var("ARCA_SERVER_PORT", "not_a_number");
        let mut config = base_config();
        let result = apply_env_overrides(&mut config);
        std::env::remove_var("ARCA_SERVER_PORT");

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("ARCA_SERVER_PORT"), "got: {msg}");
    }

    #[test]
    fn env_override_port_out_of_range_fails() {
        std::env::set_var("ARCA_SERVER_PORT", "99999");
        let mut config = base_config();
        let result = apply_env_overrides(&mut config);
        std::env::remove_var("ARCA_SERVER_PORT");

        assert!(result.is_err());
    }

    #[test]
    fn env_override_absent_leaves_defaults() {
        // Ensure none of the override vars are set.
        std::env::remove_var("ARCA_SERVER_BIND");
        std::env::remove_var("ARCA_SERVER_PORT");
        std::env::remove_var("ARCA_STORAGE_DATA_DIR");

        let mut config = base_config();
        apply_env_overrides(&mut config).unwrap();

        assert_eq!(config.server.bind, "0.0.0.0");
        assert_eq!(config.server.port, 9000);
        assert_eq!(config.storage.data_dir, "/data");
    }

    #[test]
    fn parse_config_without_limits() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.server.limits.is_none());
        assert!(config.server.cache.is_none());
    }

    #[test]
    fn parse_config_with_limits() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.limits]
max_body_size = 1000000
max_header_count = 50
max_metadata_size = 1024
rate_limit_per_second = 100
rate_limit_burst = 200
rate_limit_per_ip_per_second = 500
rate_limit_per_ip_burst = 1000
drain_timeout_seconds = 60

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let limits = config.server.limits.unwrap();
        assert_eq!(limits.max_body_size, 1_000_000);
        assert_eq!(limits.max_header_count, 50);
        assert_eq!(limits.max_metadata_size, 1024);
        assert_eq!(limits.rate_limit_per_second, 100);
        assert_eq!(limits.rate_limit_burst, 200);
        assert_eq!(limits.rate_limit_per_ip_per_second, 500);
        assert_eq!(limits.rate_limit_per_ip_burst, 1000);
        assert_eq!(limits.drain_timeout_seconds, 60);
    }

    #[test]
    fn limits_defaults() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.limits]

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let limits = config.server.limits.unwrap();
        assert_eq!(limits.max_body_size, 5_000_000_000);
        assert_eq!(limits.max_header_count, 100);
        assert_eq!(limits.max_metadata_size, 2048);
        assert_eq!(limits.rate_limit_per_second, 0);
        assert_eq!(limits.rate_limit_burst, 0);
        assert_eq!(limits.rate_limit_per_ip_per_second, 0);
        assert_eq!(limits.rate_limit_per_ip_burst, 0);
        assert_eq!(limits.drain_timeout_seconds, 30);
    }

    #[test]
    fn parse_config_with_cache() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.cache]
enabled = true
bucket_cache_size = 500
bucket_cache_ttl_seconds = 120
object_cache_size = 5000
object_cache_ttl_seconds = 15

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let cache = config.server.cache.unwrap();
        assert!(cache.enabled);
        assert_eq!(cache.bucket_cache_size, 500);
        assert_eq!(cache.bucket_cache_ttl_seconds, 120);
        assert_eq!(cache.object_cache_size, 5000);
        assert_eq!(cache.object_cache_ttl_seconds, 15);
    }

    #[test]
    fn cache_defaults() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.cache]

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let cache = config.server.cache.unwrap();
        assert!(cache.enabled);
        assert_eq!(cache.bucket_cache_size, 1000);
        assert_eq!(cache.bucket_cache_ttl_seconds, 60);
        assert_eq!(cache.object_cache_size, 10_000);
        assert_eq!(cache.object_cache_ttl_seconds, 30);
    }

    #[test]
    fn cache_disabled() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[server.cache]
enabled = false

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let cache = config.server.cache.unwrap();
        assert!(!cache.enabled);
    }

    #[test]
    fn postgres_auto_detect_from_section() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[storage.postgres]
connection_string = "postgresql://arca:arca@localhost:5432/arca"
"#;
        let mut config: Config = toml::from_str(toml_str).unwrap();
        // Before auto-detect, metadata_backend defaults to "sqlite"
        assert_eq!(config.storage.metadata_backend, "sqlite");
        // Simulate what load_config does
        if config.storage.metadata_backend == "sqlite" && config.storage.postgres.is_some() {
            config.storage.metadata_backend = "postgres".to_string();
        }
        assert_eq!(config.storage.metadata_backend, "postgres");
        assert!(config.storage.validate().is_ok());
    }

    #[test]
    fn postgres_explicit_backend_preserved() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
metadata_backend = "postgres"

[storage.postgres]
connection_string = "postgresql://arca:arca@localhost:5432/arca"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(config.storage.metadata_backend, "postgres");
        assert!(config.storage.validate().is_ok());
    }

    #[test]
    fn parse_config_without_cluster() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.cluster.is_none());
    }

    #[test]
    fn parse_config_with_cluster() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[cluster]
enabled = true
cluster_id = "arca-prod"
secret = "shared-cluster-secret"
mode = "quorum"
cluster_size = 3
discovery = "mdns"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let cluster = config.cluster.unwrap();
        assert!(cluster.enabled);
        assert_eq!(cluster.cluster_id, "arca-prod");
        assert_eq!(cluster.secret, "shared-cluster-secret");
        assert_eq!(cluster.mode, ClusterMode::Quorum);
        assert_eq!(cluster.cluster_size, Some(3));
        assert_eq!(cluster.discovery, DiscoveryMode::Mdns);
        assert!(cluster.advertise_port.is_none());
        assert!(cluster.validate().is_ok());
    }

    #[test]
    fn cluster_defaults() {
        // mode and discovery default; operational intervals default.
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[cluster]
enabled = true
cluster_id = "arca-prod"
secret = "s"
cluster_size = 3
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let cluster = config.cluster.unwrap();
        assert_eq!(cluster.mode, ClusterMode::Quorum); // default
        assert_eq!(cluster.discovery, DiscoveryMode::Mdns); // default
        assert_eq!(cluster.health_interval_seconds, 5);
        assert_eq!(cluster.anti_entropy_interval_seconds, 30);
        assert_eq!(cluster.request_timeout_seconds, 10);
        // M3: the prune window defaults to the tombstone grace.
        assert_eq!(cluster.peer_prune_days, None);
        assert_eq!(cluster.peer_prune_days(), cluster.tombstone_grace_days);
    }

    /// Baseline VALID cluster config for the validation tests; each case
    /// overrides only the field under test (functional update syntax).
    fn test_cluster_config() -> ClusterConfig {
        ClusterConfig {
            enabled: true,
            cluster_id: "c".to_string(),
            secret: "unit-test-cluster-secret-1f2e3d".to_string(),
            secret_previous: None,
            mode: ClusterMode::Quorum,
            cluster_size: Some(3),
            discovery: DiscoveryMode::Mdns,
            advertise_port: None,
            advertise_addr: None,
            seeds: vec![],
            dns_name: None,
            health_interval_seconds: 5,
            anti_entropy_interval_seconds: 30,
            request_timeout_seconds: 10,
            tombstone_grace_days: 7,
            tombstone_grace_seconds: None,
            peer_prune_days: None,
            blob_repair_budget: None,
            tls: None,
        }
    }

    fn test_cluster_tls() -> ClusterTlsConfig {
        ClusterTlsConfig {
            ca_file: "/certs/cluster/arca-cluster-ca.crt".to_string(),
            cert_file: "/certs/cluster/node.crt".to_string(),
            key_file: "/certs/cluster/node.key".to_string(),
        }
    }

    #[test]
    fn cluster_peer_prune_days_validation_and_override() {
        let mut cluster = ClusterConfig {
            peer_prune_days: Some(0),
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("peer_prune_days"), "got: {err}");

        cluster.peer_prune_days = Some(14);
        assert!(cluster.validate().is_ok());
        assert_eq!(cluster.peer_prune_days(), 14, "explicit value wins");
    }

    /// The seconds-granularity grace override (tests/demos): 0 rejected,
    /// unset → days win, set → it wins over the days knob.
    #[test]
    fn cluster_tombstone_grace_seconds_validation_and_override() {
        let mut cluster = ClusterConfig {
            tombstone_grace_seconds: Some(0),
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("tombstone_grace_seconds"), "got: {err}");

        cluster.tombstone_grace_seconds = None;
        assert!(cluster.validate().is_ok());
        assert_eq!(
            cluster.tombstone_grace(),
            std::time::Duration::from_secs(7 * 24 * 60 * 60),
            "days knob applies when the override is unset"
        );
        cluster.tombstone_grace_seconds = Some(20);
        assert!(cluster.validate().is_ok());
        assert_eq!(
            cluster.tombstone_grace(),
            std::time::Duration::from_secs(20),
            "the seconds override wins"
        );
    }

    #[test]
    fn cluster_blob_repair_budget_validation_and_default() {
        let mut cluster = ClusterConfig {
            blob_repair_budget: Some(0),
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("blob_repair_budget"), "got: {err}");

        cluster.blob_repair_budget = None;
        assert!(cluster.validate().is_ok());
        assert_eq!(cluster.blob_repair_budget(), 100, "M2 default");
        cluster.blob_repair_budget = Some(5);
        assert!(cluster.validate().is_ok());
        assert_eq!(cluster.blob_repair_budget(), 5, "explicit value wins");
    }

    #[test]
    fn lifecycle_worker_config_default_and_validation() {
        let lifecycle = LifecycleWorkerConfig::default();
        assert_eq!(lifecycle.interval_seconds, 3600);
        assert!(lifecycle.validate().is_ok());

        let zero = LifecycleWorkerConfig {
            interval_seconds: 0,
        };
        let err = zero.validate().unwrap_err().to_string();
        assert!(err.contains("interval_seconds"), "got: {err}");
    }

    #[test]
    fn cluster_quorum_requires_cluster_size() {
        let cluster = ClusterConfig {
            cluster_size: None,
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("cluster_size is required"), "got: {err}");
    }

    #[test]
    fn cluster_available_mode_ignores_cluster_size() {
        let cluster = ClusterConfig {
            mode: ClusterMode::Available,
            cluster_size: None,
            ..test_cluster_config()
        };
        assert!(cluster.validate().is_ok());
        assert_eq!(cluster.write_quorum(), None);
    }

    #[test]
    fn cluster_static_discovery_requires_seeds() {
        let cluster = ClusterConfig {
            discovery: DiscoveryMode::Static,
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("seeds is required"), "got: {err}");
    }

    #[test]
    fn cluster_dns_discovery_requires_dns_name() {
        let cluster = ClusterConfig {
            discovery: DiscoveryMode::Dns,
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("dns_name is required"), "got: {err}");
    }

    #[test]
    fn cluster_empty_secret_fails() {
        let cluster = ClusterConfig {
            secret: "   ".to_string(),
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("secret is required"), "got: {err}");
    }

    #[test]
    fn cluster_write_quorum_computation() {
        // Majority = floor(n/2)+1: 1→1, 2→2, 3→2, 4→3, 5→3.
        for (size, expected) in [(1, 1), (2, 2), (3, 2), (4, 3), (5, 3)] {
            let cluster = ClusterConfig {
                cluster_size: Some(size),
                ..test_cluster_config()
            };
            assert_eq!(cluster.write_quorum(), Some(expected), "size={size}");
        }
    }

    #[test]
    fn cluster_secret_strength_enforced() {
        // Shipped placeholders are refused outright (M5/§3.7(B)).
        for placeholder in ["dev-cluster-secret-change-me", "CHANGEME-CLUSTER-SECRET"] {
            let cluster = ClusterConfig {
                secret: placeholder.to_string(),
                ..test_cluster_config()
            };
            let err = cluster.validate().unwrap_err().to_string();
            assert!(err.contains("placeholder"), "got: {err}");
        }
        // Below the 16-char floor.
        let cluster = ClusterConfig {
            secret: "short-secret".to_string(),
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("too short"), "got: {err}");
        // Exactly 16 chars passes the floor.
        let cluster = ClusterConfig {
            secret: "0123456789abcdef".to_string(),
            ..test_cluster_config()
        };
        assert!(cluster.validate().is_ok());
    }

    #[test]
    fn cluster_secret_previous_validated() {
        // The previous secret gets the same floor as the current one (H8).
        let cluster = ClusterConfig {
            secret_previous: Some("short".to_string()),
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("secret_previous"), "got: {err}");

        // Equal to the current secret: pointless, refused.
        let base = test_cluster_config();
        let cluster = ClusterConfig {
            secret_previous: Some(base.secret.clone()),
            ..base
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("secret_previous equals secret"), "got: {err}");

        // A genuine rotation pair validates.
        let cluster = ClusterConfig {
            secret_previous: Some("previous-cluster-secret-0a1b2c".to_string()),
            ..test_cluster_config()
        };
        assert!(cluster.validate().is_ok());
    }

    #[test]
    fn cluster_secret_entropy_heuristic() {
        let weak = ClusterConfig {
            secret: "aaaaaaaaaaaaaaaaaaaa".to_string(),
            ..test_cluster_config()
        };
        assert!(weak.secret_looks_low_entropy(), "single repeated char");

        let single_class = ClusterConfig {
            secret: "abcdefghijklmnop".to_string(),
            ..test_cluster_config()
        };
        assert!(single_class.secret_looks_low_entropy(), "one character class");

        // `openssl rand -hex` output shape: two classes, many distinct chars.
        let strong = ClusterConfig {
            secret: "3f9c2a71d4e8b605a1c7".to_string(),
            ..test_cluster_config()
        };
        assert!(!strong.secret_looks_low_entropy());
    }

    #[test]
    fn cluster_tls_requires_all_files_and_server_tls() {
        // An empty member is refused.
        let cluster = ClusterConfig {
            tls: Some(ClusterTlsConfig {
                ca_file: "".to_string(),
                ..test_cluster_tls()
            }),
            ..test_cluster_config()
        };
        let err = cluster.validate().unwrap_err().to_string();
        assert!(err.contains("[cluster.tls] ca_file"), "got: {err}");

        // [cluster.tls] without [server.tls] is meaningless.
        let cluster = ClusterConfig {
            tls: Some(test_cluster_tls()),
            ..test_cluster_config()
        };
        let err = validate_cluster_transport(None, &cluster)
            .unwrap_err()
            .to_string();
        assert!(err.contains("requires [server.tls]"), "got: {err}");
    }

    #[test]
    fn cluster_over_https_requires_cluster_tls() {
        let server_tls = TlsConfig {
            cert_dir: Some("/certs".to_string()),
            cert_file: None,
            key_file: None,
            ca_file: None,
        };

        // Fail closed (R4/TD-015): an HTTPS cluster without the cluster CA
        // material refuses to start, pointing at the generator.
        let err = validate_cluster_transport(Some(&server_tls), &test_cluster_config())
            .unwrap_err()
            .to_string();
        assert!(err.contains("arca tls generate-cluster"), "got: {err}");

        // With the material it validates...
        let cluster = ClusterConfig {
            tls: Some(test_cluster_tls()),
            ..test_cluster_config()
        };
        assert!(validate_cluster_transport(Some(&server_tls), &cluster).is_ok());

        // ...unless the global client-mTLS CA is also set (one listener cannot
        // hold two client-certificate policies).
        let conflicted = TlsConfig {
            ca_file: Some("/certs/clients-ca.crt".to_string()),
            ..server_tls
        };
        let err = validate_cluster_transport(Some(&conflicted), &cluster)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mutually exclusive"), "got: {err}");

        // A plain-HTTP cluster is untouched.
        assert!(validate_cluster_transport(None, &test_cluster_config()).is_ok());
    }

    #[test]
    fn parse_config_cluster_available_static() {
        let toml_str = r#"
[server]
bind = "0.0.0.0"
port = 9000

[storage]
data_dir = "/data"

[cluster]
enabled = true
cluster_id = "arca-prod"
secret = "parse-test-cluster-secret-0a1b2c"
mode = "available"
discovery = "static"
seeds = ["arca-2:9000", "arca-3:9000"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let cluster = config.cluster.unwrap();
        assert_eq!(cluster.mode, ClusterMode::Available);
        assert_eq!(cluster.discovery, DiscoveryMode::Static);
        assert_eq!(cluster.seeds, vec!["arca-2:9000", "arca-3:9000"]);
        assert!(cluster.validate().is_ok());
    }
}
