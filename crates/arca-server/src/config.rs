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
}

/// Server configuration.
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    /// Optional domain for virtual-hosted-style requests (e.g. "s3.example.com").
    /// When set, requests to `bucket.s3.example.com` are rewritten to `/{bucket}/...`.
    pub domain: Option<String>,
    /// Optional TLS configuration. When set, the server serves HTTPS.
    pub tls: Option<TlsConfig>,
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
}

fn default_blob_prefix_depth() -> u8 {
    2
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
}

impl EncryptionConfig {
    /// Validates the encryption configuration.
    ///
    /// `master_key` is required whenever the `[encryption]` section is present
    /// (regardless of `enabled`), because per-bucket encryption needs the key
    /// even when the global default is off.
    pub fn validate(&self) -> Result<()> {
        let key = self.master_key.as_ref()
            .context("[encryption] master_key is required when the [encryption] section is present")?;
        validate_master_key(key, "master_key")?;
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

/// Loads configuration from a TOML file.
pub fn load_config(path: &Path) -> Result<Config> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading config: {}", path.display()))?;
    let config: Config =
        toml::from_str(&content).with_context(|| format!("parsing config: {}", path.display()))?;
    if let Some(tls) = &config.server.tls {
        tls.validate()?;
    }
    if let Some(enc) = &config.encryption {
        enc.validate()?;
    }
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
    fn encryption_section_requires_master_key() {
        // master_key is required whenever [encryption] section is present
        let enc = EncryptionConfig {
            enabled: true,
            master_key: None,
            previous_master_key: None,
        };
        assert!(enc.validate().is_err());

        let enc = EncryptionConfig {
            enabled: false,
            master_key: None,
            previous_master_key: None,
        };
        assert!(enc.validate().is_err());
    }

    #[test]
    fn encryption_master_key_must_be_valid_base64() {
        let enc = EncryptionConfig {
            enabled: true,
            master_key: Some("not-valid-base64!!!".to_string()),
            previous_master_key: None,
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
        };
        assert!(enc.validate().is_ok());
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
}
