//! Blob storage trait.

use crate::types::BlobId;

/// Trait for blob (binary data) storage operations.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Deletes a blob by its ID.
    async fn delete(&self, blob_id: &BlobId) -> Result<(), crate::error::ArcaError>;
}
