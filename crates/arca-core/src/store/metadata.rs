//! Metadata storage trait.

use crate::types::{BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats};

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

    /// Returns aggregate storage statistics (bucket count, object count, total size).
    async fn get_stats(&self) -> Result<StorageStats, crate::error::ArcaError>;

    // -- Object operations --

    /// Inserts or replaces an object record. Returns `(old_record, version_id)`:
    /// - `old_record`: the overwritten record (if any), so the caller can delete the orphaned blob.
    /// - `version_id`: the version ID assigned to the new record (None for unversioned).
    async fn put_object(
        &self,
        record: &ObjectRecord,
    ) -> Result<(Option<ObjectRecord>, Option<String>), crate::error::ArcaError>;

    /// Returns the object record for the given bucket/key, or None if not found.
    /// Filters out delete markers (use `get_latest_object` to include them).
    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Returns the latest version of an object (including delete markers).
    /// Used by GET/HEAD to detect delete markers and return appropriate headers.
    async fn get_latest_object(
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

    // -- Versioned object operations --

    /// Returns a specific version of an object, or None if not found.
    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Hard-deletes a specific object version. Returns the deleted record.
    /// If the deleted version was `is_latest`, promotes the next-newest version.
    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Lists all versions of objects in a bucket, including delete markers,
    /// ordered by `(key ASC, last_modified DESC)`.
    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        version_id_marker: Option<&str>,
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

    // -- Object Lock operations --

    /// Sets the retention mode and retain-until-date on an object version.
    /// Returns true if the object was found and updated, false if not found.
    async fn set_object_retention(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        retention_mode: Option<&str>,
        retain_until_date: Option<&str>,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Sets the legal hold status on an object version.
    /// Returns true if the object was found and updated, false if not found.
    async fn set_object_legal_hold(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool, crate::error::ArcaError>;

    // -- Bucket config operations --

    /// Gets a bucket configuration value.
    async fn get_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<Option<String>, crate::error::ArcaError>;

    /// Sets a bucket configuration value.
    async fn set_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
    ) -> Result<(), crate::error::ArcaError>;

    /// Deletes a bucket configuration value.
    async fn delete_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<bool, crate::error::ArcaError>;

    // -- Tag operations --

    /// Gets all tags for a bucket.
    async fn get_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<Vec<(String, String)>, crate::error::ArcaError>;

    /// Replaces all tags for a bucket.
    async fn put_bucket_tags(
        &self,
        bucket: &str,
        tags: &[(String, String)],
    ) -> Result<(), crate::error::ArcaError>;

    /// Deletes all tags for a bucket.
    async fn delete_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Gets all tags for an object (version_id="" for unversioned).
    async fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Vec<(String, String)>, crate::error::ArcaError>;

    /// Replaces all tags for an object.
    async fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        tags: &[(String, String)],
    ) -> Result<(), crate::error::ArcaError>;

    /// Deletes all tags for an object.
    async fn delete_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<bool, crate::error::ArcaError>;

    // -- Multipart upload operations --

    // -- Lifecycle query operations --

    /// Lists objects whose last_modified is before cutoff, filtered by prefix
    /// and optional tags. For lifecycle expiration evaluation.
    /// Only returns `is_latest = 1` and `is_delete_marker = 0` objects.
    async fn list_expired_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        tags: &[(String, String)],
        cutoff: chrono::DateTime<chrono::Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, crate::error::ArcaError>;

    /// Lists noncurrent (is_latest=0, is_delete_marker=0) object versions
    /// whose last_modified is before cutoff. For NoncurrentVersionExpiration.
    async fn list_noncurrent_expired_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        cutoff: chrono::DateTime<chrono::Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, crate::error::ArcaError>;

    /// Lists multipart uploads initiated before the cutoff date.
    async fn list_stale_multipart_uploads(
        &self,
        bucket: &str,
        cutoff: chrono::DateTime<chrono::Utc>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, crate::error::ArcaError>;

    /// Lists in-progress multipart uploads for a bucket.
    ///
    /// - `prefix`: only return uploads whose key starts with this prefix.
    /// - `key_marker`: only return uploads whose key is lexicographically after this value.
    /// - `upload_id_marker`: when `key_marker` matches a key exactly, skip uploads with
    ///   upload_id <= this value (for pagination within a key).
    /// - `max_uploads`: maximum number of uploads to return.
    async fn list_multipart_uploads(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        upload_id_marker: Option<&str>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, crate::error::ArcaError>;

    // -- Cluster replication (Phase 29 HA) --

    /// Applies a fully-formed object version received verbatim from a cluster
    /// peer, idempotently. Unlike [`MetadataStore::put_object`], it does NOT
    /// mint a `version_id` and does NOT run bucket versioning logic: the row is
    /// stored as-is, then `is_latest` is recomputed deterministically across the
    /// key's versions. Conflict resolution is last-write-wins on
    /// `(last_modified, version_id, blob_id)`, so all nodes converge on the same
    /// current version without coordination.
    ///
    /// Default implementation: unsupported (for non-clustered backends).
    async fn apply_remote_object(
        &self,
        _record: &ObjectRecord,
    ) -> Result<(), crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "apply_remote_object: cluster replication is not supported by this backend".to_string(),
        ))
    }

    /// Applies a replicated hard-delete of a specific object version, then
    /// recomputes `is_latest`. `version_id == "null"` targets the null-version
    /// row. Idempotent (deleting an absent version is a no-op).
    ///
    /// Default implementation: unsupported.
    async fn apply_remote_version_delete(
        &self,
        _bucket: &str,
        _key: &str,
        _version_id: &str,
    ) -> Result<(), crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "apply_remote_version_delete: cluster replication is not supported by this backend"
                .to_string(),
        ))
    }

    /// Creates or replaces a bucket row verbatim (name, created_at, owner) from
    /// a control-plane replication op. Idempotent: re-delivery overwrites with
    /// the same values. Unlike [`MetadataStore::create_bucket`] it preserves the
    /// origin's `created_at`/`owner` and never errors on an existing bucket, so
    /// replicated objects become servable on the peer.
    ///
    /// Default implementation: unsupported.
    async fn apply_remote_bucket(
        &self,
        _info: &BucketInfo,
    ) -> Result<(), crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "apply_remote_bucket: cluster replication is not supported by this backend".to_string(),
        ))
    }

    /// Creates an in-progress multipart upload row verbatim from a control-plane
    /// replication op. Idempotent: a multipart upload is immutable once created
    /// (its `upload_id` is the key), so re-delivery is a no-op (INSERT-or-ignore)
    /// rather than the plain INSERT of [`MetadataStore::create_multipart_upload`].
    /// Replicating it lets any node accept `UploadPart` / `CompleteMultipartUpload`
    /// for an upload initiated on a peer.
    ///
    /// Default implementation: unsupported.
    async fn apply_remote_multipart_upload(
        &self,
        _record: &MultipartUploadRecord,
    ) -> Result<(), crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "apply_remote_multipart_upload: cluster replication is not supported by this backend"
                .to_string(),
        ))
    }
}
