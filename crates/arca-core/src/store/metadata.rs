//! Metadata storage trait.

use crate::types::{BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord};

/// Trait for metadata storage operations.
#[async_trait::async_trait]
pub trait MetadataStore: Send + Sync {
    // -- Bucket operations --

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

    /// Returns true if the bucket has no objects.
    async fn bucket_is_empty(&self, name: &str) -> Result<bool, crate::error::ArcaError>;

    // -- Object operations --

    /// Inserts or replaces an object record. Returns the old record if one was overwritten
    /// (so the caller can delete the orphaned blob).
    async fn put_object(
        &self,
        record: &ObjectRecord,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Returns the object record for the given bucket/key, or None if not found.
    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Deletes an object record. Returns the deleted record if it existed
    /// (so the caller can delete the orphaned blob).
    async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Lists objects in a bucket, ordered by key.
    ///
    /// - `prefix`: only return keys starting with this prefix.
    /// - `start_after`: only return keys lexicographically after this value.
    /// - `max_keys`: maximum number of records to return.
    async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, crate::error::ArcaError>;

    // -- Multipart upload operations --

    /// Creates a new multipart upload record.
    async fn create_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), crate::error::ArcaError>;

    /// Returns the multipart upload record for the given upload_id, or None if not found.
    async fn get_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Option<MultipartUploadRecord>, crate::error::ArcaError>;

    /// Inserts or replaces a part record. Returns the old part if one was overwritten
    /// (so the caller can delete the orphaned blob).
    async fn put_part(
        &self,
        part: &PartRecord,
    ) -> Result<Option<PartRecord>, crate::error::ArcaError>;

    /// Lists all parts for a multipart upload, ordered by part_number.
    async fn list_parts(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, crate::error::ArcaError>;

    /// Deletes a multipart upload and all its parts. Returns the deleted parts
    /// (so the caller can delete the orphaned blobs).
    async fn delete_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, crate::error::ArcaError>;
}
