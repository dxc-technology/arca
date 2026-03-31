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
    /// Optional TLS configuration. When set, the server serves HTTPS.
    pub tls: Option<TlsConfig>,
    /// Request limits and rate limiting configuration.
    pub limits: Option<LimitsConfig>,
    /// In-memory metadata cache configuration.
    pub cache: Option<CacheConfig>,
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
#[derive(Debug, Deserialize)]
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
}

fn default_blob_prefix_depth() -> u8 {
    2
}

fn default_metadata_backend() -> String {
    "sqlite".to_string()
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
    if let Some(tls) = &config.server.tls {
        tls.validate()?;
    }
    if let Some(enc) = &config.encryption {
        enc.validate()?;
    }
    config.storage.validate()?;
    Ok(config)
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
                tls: None,
                limits: None,
                cache: None,
            },
            storage: StorageConfig {
                data_dir: "/data".to_string(),
                blob_prefix_depth: 2,
                metadata_backend: "sqlite".to_string(),
                postgres: None,
            },
            encryption: None,
            monitoring: None,
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
}
