//! Presigned URL tracking storage trait.
//!
//! Stores metadata about generated presigned URLs for visibility in the console.
//! The actual URL is NOT stored (security: avoid persisting bearer tokens).
//! Only tracking metadata is kept: bucket, key, method, expiry, creator.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Metadata record for a generated presigned URL.
///
/// The full URL is intentionally NOT stored to avoid persisting bearer tokens
/// in the database. If the URL is needed again, generate a new one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresignedUrlRecord {
    /// Unique record ID (UUID).
    pub id: String,
    /// Bucket the presigned URL grants access to.
    pub bucket: String,
    /// Object key the presigned URL grants access to.
    pub key: String,
    /// HTTP method (GET, PUT, HEAD, DELETE).
    pub method: String,
    /// Original duration in seconds (e.g. 3600 for 1 hour).
    pub expires_seconds: u64,
    /// When the presigned URL was generated.
    pub created_at: DateTime<Utc>,
    /// When the presigned URL expires.
    pub expires_at: DateTime<Utc>,
    /// Access key ID of the credential that signed the URL.
    pub access_key_id: String,
}

/// Trait for presigned URL tracking operations.
#[async_trait::async_trait]
pub trait PresignedUrlStore: Send + Sync {
    /// Insert a presigned URL tracking record.
    async fn insert_presigned_url(
        &self,
        record: &PresignedUrlRecord,
    ) -> Result<(), crate::error::ArcaError>;

    /// List active (non-expired) presigned URLs for a bucket.
    /// Returns records ordered by creation time (newest first).
    async fn list_presigned_urls(
        &self,
        bucket: &str,
    ) -> Result<Vec<PresignedUrlRecord>, crate::error::ArcaError>;

    /// Delete a presigned URL tracking record by ID.
    /// Returns `true` if a record was actually deleted.
    async fn delete_presigned_url(
        &self,
        id: &str,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Delete all expired presigned URL records.
    /// Returns the number of deleted records.
    async fn purge_expired_presigned_urls(
        &self,
    ) -> Result<u64, crate::error::ArcaError>;
}
