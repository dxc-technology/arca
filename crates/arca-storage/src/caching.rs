//! Caching metadata store wrapper.
//!
//! Wraps any `MetadataStore` implementation with an in-memory LRU cache for
//! frequently-accessed bucket existence and object HEAD lookups.
//! Write operations delegate to the inner store and invalidate the cache.

use std::sync::Arc;
use std::time::Duration;

use moka::future::Cache;

use arca_core::error::ArcaError;
use arca_core::store::MetadataStore;
use arca_core::types::{
    BlobId, BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats,
};

/// A `MetadataStore` wrapper that caches `head_bucket` and `get_object`
/// / `get_latest_object` results in memory using LRU eviction and TTL.
pub struct CachingMetadataStore {
    inner: Arc<dyn MetadataStore>,
    bucket_cache: Cache<String, Option<BucketInfo>>,
    object_cache: Cache<String, Option<ObjectRecord>>,
}

impl CachingMetadataStore {
    /// Creates a new caching wrapper around `inner`.
    pub fn new(
        inner: Arc<dyn MetadataStore>,
        bucket_cache_size: u64,
        bucket_cache_ttl_seconds: u64,
        object_cache_size: u64,
        object_cache_ttl_seconds: u64,
    ) -> Self {
        let bucket_cache = Cache::builder()
            .max_capacity(bucket_cache_size)
            .time_to_live(Duration::from_secs(bucket_cache_ttl_seconds))
            .build();

        let object_cache = Cache::builder()
            .max_capacity(object_cache_size)
            .time_to_live(Duration::from_secs(object_cache_ttl_seconds))
            .build();

        Self {
            inner,
            bucket_cache,
            object_cache,
        }
    }

    /// Cache key for objects: "bucket\0key".
    fn object_key(bucket: &str, key: &str) -> String {
        format!("{bucket}\0{key}")
    }

    /// Invalidate bucket cache entry (used on bucket create/delete).
    async fn invalidate_bucket(&self, bucket: &str) {
        self.bucket_cache.invalidate(bucket).await;
    }
}

#[async_trait::async_trait]
impl MetadataStore for CachingMetadataStore {
    // -- Bucket operations (cached) --

    async fn list_buckets(&self) -> Result<Vec<BucketInfo>, ArcaError> {
        self.inner.list_buckets().await
    }

    async fn create_bucket(&self, name: &str) -> Result<(), ArcaError> {
        let result = self.inner.create_bucket(name).await;
        if result.is_ok() {
            self.invalidate_bucket(name).await;
        }
        result
    }

    async fn head_bucket(&self, name: &str) -> Result<Option<BucketInfo>, ArcaError> {
        let key = name.to_string();
        if let Some(cached) = self.bucket_cache.get(&key).await {
            return Ok(cached);
        }
        let result = self.inner.head_bucket(name).await?;
        self.bucket_cache.insert(key, result.clone()).await;
        Ok(result)
    }

    async fn delete_bucket(&self, name: &str) -> Result<bool, ArcaError> {
        let result = self.inner.delete_bucket(name).await;
        if result.is_ok() {
            self.invalidate_bucket(name).await;
        }
        result
    }

    async fn bucket_is_empty(&self, name: &str) -> Result<bool, ArcaError> {
        self.inner.bucket_is_empty(name).await
    }

    async fn get_stats(&self) -> Result<StorageStats, ArcaError> {
        self.inner.get_stats().await
    }

    // -- Object operations (cached for get, invalidated on write) --

    async fn put_object(
        &self,
        record: &ObjectRecord,
    ) -> Result<(Option<ObjectRecord>, Option<String>), ArcaError> {
        let result = self.inner.put_object(record).await;
        if result.is_ok() {
            let key = Self::object_key(&record.bucket, &record.key);
            self.object_cache.invalidate(&key).await;
        }
        result
    }

    async fn get_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        // We cache get_latest_object, not get_object (which filters delete markers).
        // Delegate directly to avoid stale cache entries with different semantics.
        self.inner.get_object(bucket, key).await
    }

    async fn get_latest_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let cache_key = Self::object_key(bucket, key);
        if let Some(cached) = self.object_cache.get(&cache_key).await {
            return Ok(cached);
        }
        let result = self.inner.get_latest_object(bucket, key).await?;
        self.object_cache.insert(cache_key, result.clone()).await;
        Ok(result)
    }

    async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let result = self.inner.delete_object(bucket, key).await;
        if result.is_ok() {
            let cache_key = Self::object_key(bucket, key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    async fn list_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_objects(bucket, prefix, start_after, max_keys)
            .await
    }

    // -- Versioned object operations --

    async fn get_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        self.inner.get_object_version(bucket, key, version_id).await
    }

    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, ArcaError> {
        let result = self
            .inner
            .delete_object_version(bucket, key, version_id)
            .await;
        if result.is_ok() {
            let cache_key = Self::object_key(bucket, key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    async fn list_object_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        version_id_marker: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_object_versions(bucket, prefix, key_marker, version_id_marker, max_keys)
            .await
    }

    // -- Cluster replication (Phase 29 HA): forward + invalidate --
    //
    // Without these overrides the default trait impls ("unsupported") would
    // shadow the backing store. Forwarding to the inner store applies the
    // replicated change; invalidating the key drops any stale cached read so
    // the next `get_latest_object` reflects the peer's write.

    async fn apply_remote_object(&self, record: &ObjectRecord) -> Result<(), ArcaError> {
        let result = self.inner.apply_remote_object(record).await;
        if result.is_ok() {
            let cache_key = Self::object_key(&record.bucket, &record.key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    async fn apply_remote_version_delete(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), ArcaError> {
        let result = self
            .inner
            .apply_remote_version_delete(bucket, key, version_id)
            .await;
        if result.is_ok() {
            let cache_key = Self::object_key(bucket, key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    async fn apply_remote_bucket(&self, info: &BucketInfo) -> Result<(), ArcaError> {
        let result = self.inner.apply_remote_bucket(info).await;
        if result.is_ok() {
            // Drop any stale head_bucket cache entry so the replicated row is seen.
            self.invalidate_bucket(&info.name).await;
        }
        result
    }

    async fn apply_remote_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        // Multipart upload rows aren't cached; forward to the inner store so the
        // default "unsupported" impl never shadows a clustered backend.
        self.inner.apply_remote_multipart_upload(record).await
    }

    async fn list_rows_changed_since(
        &self,
        since: u64,
        limit: u32,
    ) -> Result<Vec<(u64, ObjectRecord)>, ArcaError> {
        // Pure read of the inner store's changed-since cursor; nothing to cache.
        // Forwarded so the default "unsupported" impl never shadows a clustered
        // backend behind the cache.
        self.inner.list_rows_changed_since(since, limit).await
    }

    async fn current_object_seq(&self) -> Result<u64, ArcaError> {
        // Pure read of the write counter; forwarded so the default `0` never
        // shadows a clustered backend behind the cache.
        self.inner.current_object_seq().await
    }

    async fn seed_object_seq_to_max(&self) -> Result<u64, ArcaError> {
        // Counter reconciliation; forwarded so the default no-op doesn't shadow
        // the backend behind the cache (no cached state depends on the counter).
        self.inner.seed_object_seq_to_max().await
    }

    async fn purge_tombstones(
        &self,
        before: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, ArcaError> {
        // Maintenance delete; forwarded so the default no-op doesn't shadow the
        // backend's real GC behind the cache.
        self.inner.purge_tombstones(before).await
    }

    async fn list_referenced_blob_ids(&self) -> Result<Vec<arca_core::types::BlobId>, ArcaError> {
        // Pure read for the cluster blob repair/GC scan; forwarded so the default
        // "unsupported" impl never shadows a clustered backend behind the cache.
        self.inner.list_referenced_blob_ids().await
    }

    // -- Multipart upload operations (delegated) --

    async fn create_multipart_upload(
        &self,
        record: &MultipartUploadRecord,
    ) -> Result<(), ArcaError> {
        self.inner.create_multipart_upload(record).await
    }

    async fn get_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Option<MultipartUploadRecord>, ArcaError> {
        self.inner.get_multipart_upload(upload_id).await
    }

    async fn put_part(
        &self,
        part: &PartRecord,
    ) -> Result<Option<PartRecord>, ArcaError> {
        self.inner.put_part(part).await
    }

    async fn list_parts(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, ArcaError> {
        self.inner.list_parts(upload_id).await
    }

    async fn delete_multipart_upload(
        &self,
        upload_id: &str,
    ) -> Result<Vec<PartRecord>, ArcaError> {
        self.inner.delete_multipart_upload(upload_id).await
    }

    // -- Object Lock operations (delegated, invalidate object cache) --

    async fn set_object_retention(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        retention_mode: Option<&str>,
        retain_until_date: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let result = self
            .inner
            .set_object_retention(bucket, key, version_id, retention_mode, retain_until_date)
            .await;
        if result.is_ok() {
            let cache_key = Self::object_key(bucket, key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    async fn set_object_legal_hold(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let result = self
            .inner
            .set_object_legal_hold(bucket, key, version_id, status)
            .await;
        if result.is_ok() {
            let cache_key = Self::object_key(bucket, key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    // -- Re-encryption (delegated, invalidate object cache) --

    async fn update_object_encryption(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        algorithm: Option<&str>,
        key_id: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let result = self
            .inner
            .update_object_encryption(bucket, key, version_id, algorithm, key_id)
            .await;
        if result.is_ok() {
            let cache_key = Self::object_key(bucket, key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    async fn update_object_encryption_cas(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        old_blob_id: &BlobId,
        new_blob_id: &BlobId,
        algorithm: Option<&str>,
        key_id: Option<&str>,
    ) -> Result<bool, ArcaError> {
        let result = self
            .inner
            .update_object_encryption_cas(
                bucket, key, version_id, old_blob_id, new_blob_id, algorithm, key_id,
            )
            .await;
        if result.is_ok() {
            let cache_key = Self::object_key(bucket, key);
            self.object_cache.invalidate(&cache_key).await;
        }
        result
    }

    // -- Bucket config operations (delegated) --

    async fn get_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<Option<String>, ArcaError> {
        self.inner.get_bucket_config(bucket, config_key).await
    }

    async fn set_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
    ) -> Result<(), ArcaError> {
        self.inner
            .set_bucket_config(bucket, config_key, config_value)
            .await
    }

    async fn delete_bucket_config(
        &self,
        bucket: &str,
        config_key: &str,
    ) -> Result<bool, ArcaError> {
        self.inner.delete_bucket_config(bucket, config_key).await
    }

    async fn apply_bucket_config_at(
        &self,
        bucket: &str,
        config_key: &str,
        config_value: &str,
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), ArcaError> {
        self.inner
            .apply_bucket_config_at(bucket, config_key, config_value, updated_at)
            .await
    }

    // -- Tag operations (delegated) --

    async fn get_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<Vec<(String, String)>, ArcaError> {
        self.inner.get_bucket_tags(bucket).await
    }

    async fn put_bucket_tags(
        &self,
        bucket: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        self.inner.put_bucket_tags(bucket, tags).await
    }

    async fn delete_bucket_tags(
        &self,
        bucket: &str,
    ) -> Result<bool, ArcaError> {
        self.inner.delete_bucket_tags(bucket).await
    }

    async fn apply_bucket_tags_at(
        &self,
        bucket: &str,
        tags: &[(String, String)],
        updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), ArcaError> {
        self.inner.apply_bucket_tags_at(bucket, tags, updated_at).await
    }

    async fn get_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Vec<(String, String)>, ArcaError> {
        self.inner.get_object_tags(bucket, key, version_id).await
    }

    async fn put_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        tags: &[(String, String)],
    ) -> Result<(), ArcaError> {
        self.inner
            .put_object_tags(bucket, key, version_id, tags)
            .await
    }

    async fn delete_object_tags(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<bool, ArcaError> {
        self.inner
            .delete_object_tags(bucket, key, version_id)
            .await
    }

    // -- Lifecycle query operations (delegated) --

    async fn list_expired_objects(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        tags: &[(String, String)],
        cutoff: chrono::DateTime<chrono::Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_expired_objects(bucket, prefix, tags, cutoff, start_after, max_keys)
            .await
    }

    async fn list_noncurrent_expired_versions(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        cutoff: chrono::DateTime<chrono::Utc>,
        start_after: Option<&str>,
        max_keys: u32,
    ) -> Result<Vec<ObjectRecord>, ArcaError> {
        self.inner
            .list_noncurrent_expired_versions(bucket, prefix, cutoff, start_after, max_keys)
            .await
    }

    async fn list_stale_multipart_uploads(
        &self,
        bucket: &str,
        cutoff: chrono::DateTime<chrono::Utc>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        self.inner
            .list_stale_multipart_uploads(bucket, cutoff, max_uploads)
            .await
    }

    async fn list_multipart_uploads(
        &self,
        bucket: &str,
        prefix: Option<&str>,
        key_marker: Option<&str>,
        upload_id_marker: Option<&str>,
        max_uploads: u32,
    ) -> Result<Vec<MultipartUploadRecord>, ArcaError> {
        self.inner
            .list_multipart_uploads(bucket, prefix, key_marker, upload_id_marker, max_uploads)
            .await
    }
}
