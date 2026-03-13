//! Shared application state for the Axum router.

use std::sync::Arc;

use arca_core::store::{BlobStore, CredentialStore, MetadataStore};

/// Application state shared across all handlers.
#[derive(Clone)]
pub struct AppState {
    pub metadata: Arc<dyn MetadataStore>,
    pub blob: Arc<dyn BlobStore>,
    pub credentials: Arc<dyn CredentialStore>,
    pub domain: Option<String>,
    pub started_at: std::time::Instant,
    pub version: String,
    pub tls_enabled: bool,
    /// Whether server-side encryption is enabled by default for new objects.
    pub encryption_enabled: bool,
}
