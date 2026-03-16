//! Shared application state for the Axum router.

use std::path::PathBuf;
use std::sync::Arc;

use arca_core::store::{BlobStore, CredentialStore, MetadataStore};

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
    pub credentials: Arc<dyn CredentialStore>,
    pub domain: Option<String>,
    pub started_at: std::time::Instant,
    pub version: String,
    pub tls_enabled: bool,
    /// Whether server-side encryption is enabled by default for new objects.
    pub encryption_enabled: bool,
    /// KMS provider: "local" (config file), "vault" (Vault/OpenBAO), or None.
    pub kms_provider: Option<String>,
    /// KMS endpoint URL (only when kms_provider = "vault").
    pub kms_endpoint: Option<String>,
    /// Data directories (for filesystem stats). Multiple entries for multi-volume setups.
    pub data_dirs: Vec<PathBuf>,
}

impl AppState {
    /// Returns the appropriate blob store for writing to a specific bucket.
    ///
    /// Checks per-bucket encryption config and the global default to decide
    /// whether to write through the encrypting store or the plain store.
    pub async fn blob_for_write(&self, bucket: &str) -> Arc<dyn BlobStore> {
        let should_encrypt = if self.encryption_enabled {
            // Global encryption is on — all buckets are encrypted
            true
        } else {
            // Global encryption is off — check per-bucket config
            matches!(
                self.metadata.get_bucket_config(bucket, "encryption_algorithm").await,
                Ok(Some(_))
            )
        };

        if should_encrypt {
            // state.blob is EncryptingBlobStore when key is available
            self.blob.clone()
        } else {
            // Use plain store if available, otherwise state.blob (which is FsBlobStore)
            self.plain_blob.clone().unwrap_or_else(|| self.blob.clone())
        }
    }
}
