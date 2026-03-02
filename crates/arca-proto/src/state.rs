//! Shared application state for the Axum router.

use std::sync::Arc;

use arca_core::store::MetadataStore;

/// Application state shared across all handlers.
#[derive(Clone)]
pub struct AppState {
    pub metadata: Arc<dyn MetadataStore>,
}
