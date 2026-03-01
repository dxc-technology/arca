//! Metadata storage trait.

use crate::types::BucketInfo;

/// Trait for metadata storage operations.
#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync {
    /// Lists all buckets.
    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, crate::error::ArcaError>;
}
