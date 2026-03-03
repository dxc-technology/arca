//! Configuration loading and types.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Top-level configuration.
#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub storage: StorageConfig,
}

/// Server configuration.
#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    pub port: u16,
    /// Optional domain for virtual-hosted-style requests (e.g. "s3.example.com").
    /// When set, requests to `bucket.s3.example.com` are rewritten to `/{bucket}/...`.
    pub domain: Option<String>,
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

/// Loads configuration from a TOML file.
pub fn load_config(path: &Path) -> Result<Config> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading config: {}", path.display()))?;
    let config: Config =
        toml::from_str(&content).with_context(|| format!("parsing config: {}", path.display()))?;
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
}
