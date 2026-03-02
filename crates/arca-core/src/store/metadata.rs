//! Metadata storage trait.

use crate::types::BucketInfo;

/// Trait for metadata storage operations.
#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync {
    /// Lists all buckets, ordered by name.
    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, crate::error::ArcaError>;

    /// Creates a new bucket. Returns error if the bucket already exists.
    async fn create_bucket(&self, name: &str) -> Result<(), crate::error::ArcaError>;

    /// Returns bucket info if it exists, None otherwise.
    async fn head_bucket(
        &self,
        name: &str,
    ) -> Result<Option<BucketInfo>, crate::error::ArcaError>;

    /// Deletes a bucket. Returns true if it existed, false otherwise.
    async fn delete_bucket(&self, name: &str) -> Result<bool, crate::error::ArcaError>;
}
