//! Metadata storage trait.

use crate::error::S3ErrorCode;
use crate::s3::etag::etag_matches;
use crate::types::{BucketInfo, MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats};

/// Compare-and-swap preconditions for a write (S3 `If-Match` /
/// `If-None-Match`), evaluated inside the same transaction that installs the
/// new version — see `.claude/plans/arca-conditional-write-atomicity.md`.
#[derive(Debug, Clone, Default)]
pub struct WritePrecondition {
    /// `If-Match`: proceed only if the current latest object exists and its
    /// ETag matches one of the listed values. `*` matches any existing object.
    pub if_match: Option<String>,
    /// `If-None-Match`: proceed only if no current object matches. `*` means
    /// "only if the object does not exist".
    pub if_none_match: Option<String>,
}

impl WritePrecondition {
    /// True when neither header was given — the write is unconditional.
    pub fn is_empty(&self) -> bool {
        self.if_match.is_none() && self.if_none_match.is_none()
    }

    /// Evaluates the precondition against the current object as
    /// [`MetadataStore::get_object`] would see it (latest version, delete
    /// markers filtered out). Returns `Err(NoSuchKey)` when `if_match` was
    /// given and no current object exists, `Err(PreconditionFailed)` on a
    /// mismatch, `Ok(())` otherwise.
    pub fn evaluate(&self, current: Option<&ObjectRecord>) -> Result<(), S3ErrorCode> {
        if let Some(ref expected) = self.if_match {
            match current {
                Some(obj) => {
                    let quoted = format!("\"{}\"", obj.etag);
                    if !etag_matches(expected, &quoted) {
                        return Err(S3ErrorCode::PreconditionFailed);
                    }
                }
                None => return Err(S3ErrorCode::NoSuchKey),
            }
        }
        if let Some(ref expected) = self.if_none_match {
            if let Some(obj) = current {
                let quoted = format!("\"{}\"", obj.etag);
                if etag_matches(expected, &quoted) {
                    return Err(S3ErrorCode::PreconditionFailed);
                }
            }
        }
        Ok(())
    }
}

/// Preconditions for a conditional delete (S3 adds size and last-modified
/// time to `If-Match` for `DeleteObject`).
#[derive(Debug, Clone, Default)]
pub struct DeletePrecondition {
    pub if_match: Option<String>,
    pub if_match_last_modified: Option<chrono::DateTime<chrono::Utc>>,
    pub if_match_size: Option<u64>,
}

impl DeletePrecondition {
    /// True when no conditional header was given — the delete is unconditional.
    pub fn is_empty(&self) -> bool {
        self.if_match.is_none()
            && self.if_match_last_modified.is_none()
            && self.if_match_size.is_none()
    }

    /// Evaluates the precondition against the object being deleted (as
    /// [`MetadataStore::get_latest_object`] would see it — includes delete
    /// markers). A missing object is always `Ok(())`: `DeleteObject` on an
    /// absent key is a no-op, never a precondition failure.
    pub fn evaluate(&self, current: Option<&ObjectRecord>) -> Result<(), S3ErrorCode> {
        let Some(obj) = current else {
            return Ok(());
        };
        if let Some(ref expected) = self.if_match {
            let quoted = format!("\"{}\"", obj.etag);
            if !etag_matches(expected, &quoted) {
                return Err(S3ErrorCode::PreconditionFailed);
            }
        }
        if let Some(expected) = self.if_match_last_modified {
            if obj.last_modified.timestamp() != expected.timestamp() {
                return Err(S3ErrorCode::PreconditionFailed);
            }
        }
        if let Some(expected) = self.if_match_size {
            if obj.size != expected {
                return Err(S3ErrorCode::PreconditionFailed);
            }
        }
        Ok(())
    }
}

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

    /// Same as [`MetadataStore::put_object`], but applies the write only if
    /// `pre` holds against the current object (see
    /// [`WritePrecondition::evaluate`]), checked inside the same transaction
    /// that installs the new version — this is the authoritative
    /// compare-and-swap, not a check-then-commit race. On a mismatch nothing
    /// is written and the error is `ArcaError::S3` with
    /// `S3ErrorCode::PreconditionFailed` (or `NoSuchKey` when `if_match` was
    /// given and no current object exists).
    async fn put_object_if(
        &self,
        record: &ObjectRecord,
        pre: &WritePrecondition,
    ) -> Result<(Option<ObjectRecord>, Option<String>), crate::error::ArcaError>;

    /// Inserts or replaces an object record unconditionally. Returns
    /// `(old_record, version_id)`:
    /// - `old_record`: the overwritten record (if any), so the caller can delete the orphaned blob.
    /// - `version_id`: the version ID assigned to the new record (None for unversioned).
    async fn put_object(
        &self,
        record: &ObjectRecord,
    ) -> Result<(Option<ObjectRecord>, Option<String>), crate::error::ArcaError> {
        self.put_object_if(record, &WritePrecondition::default())
            .await
    }

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

    /// Same as [`MetadataStore::delete_object`], but applies the delete only
    /// if `pre` holds against the object being deleted (see
    /// [`DeletePrecondition::evaluate`]), checked inside the same transaction
    /// that installs the delete/tombstone/delete-marker. On a mismatch
    /// nothing is deleted and the error is `ArcaError::S3` with
    /// `S3ErrorCode::PreconditionFailed`.
    async fn delete_object_if(
        &self,
        bucket: &str,
        key: &str,
        pre: &DeletePrecondition,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Deletes an object record unconditionally. Returns the deleted record
    /// if it existed (so the caller can delete the orphaned blob).
    async fn delete_object(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError> {
        self.delete_object_if(bucket, key, &DeletePrecondition::default())
            .await
    }

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

    /// Same as [`MetadataStore::delete_object_version`], but applies the
    /// delete only if `pre` holds against the specific version being deleted.
    /// Per S3 semantics (and the existing single-version delete handler), the
    /// precondition is skipped entirely — never evaluated, never refused —
    /// when the targeted version is itself a delete marker.
    async fn delete_object_version_if(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
        pre: &DeletePrecondition,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError>;

    /// Hard-deletes a specific object version unconditionally. Returns the
    /// deleted record. If the deleted version was `is_latest`, promotes the
    /// next-newest version.
    async fn delete_object_version(
        &self,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<Option<ObjectRecord>, crate::error::ArcaError> {
        self.delete_object_version_if(bucket, key, version_id, &DeletePrecondition::default())
            .await
    }

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

    // -- Re-encryption (Phase 30 maintenance jobs) --

    /// Updates the encryption metadata of an object version IN PLACE (same
    /// `blob_id`): sets `encryption_algorithm`/`encryption_key_id` to the given
    /// values, or clears both with `None` (after a decrypt). Bumps the
    /// node-local `seq` so the change reaches cluster peers via the anti-entropy
    /// manifest; leaves `last_modified` and the ETag untouched — re-encryption
    /// never changes the logical object. `version_id == None` targets the
    /// current version. Returns whether a row was updated.
    ///
    /// Used by the maintenance-mode (S3-drained) re-encryption path, where the
    /// blob file is rewritten under the same `blob_id`. In a cluster this MUST
    /// be paired with re-replication of the rewritten bytes; the hot/cluster
    /// path instead uses the copy-on-write variant
    /// [`MetadataStore::update_object_encryption_cas`].
    async fn update_object_encryption(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        algorithm: Option<&str>,
        key_id: Option<&str>,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Copy-on-write swap of an object version's blob: atomically sets
    /// `blob_id = new_blob_id`, `encryption_algorithm`, `encryption_key_id` and
    /// a fresh `seq`, but ONLY IF the row still references `old_blob_id`
    /// (compare-and-swap guard). Returns `true` when exactly that row was
    /// updated, `false` when a concurrent client write already replaced the blob
    /// (the caller then discards the freshly written blob and skips).
    /// `version_id == None` targets the current version; `last_modified` and the
    /// ETag are preserved.
    ///
    /// This is the hot-path, cluster-safe primitive: changing `blob_id` makes
    /// anti-entropy / read-repair pull the new (encrypted) blob with its sidecar
    /// to every peer, rather than leaving peers with stale bytes under an
    /// unchanged id.
    async fn update_object_encryption_cas(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        old_blob_id: &crate::types::BlobId,
        new_blob_id: &crate::types::BlobId,
        algorithm: Option<&str>,
        key_id: Option<&str>,
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

    /// Upserts a bucket-config key preserving the given `updated_at` verbatim
    /// (the LWW key of the control-plane reconcile, R5/TD-016) — unlike
    /// [`MetadataStore::set_bucket_config`], which stamps `now()`.
    ///
    /// Default implementation: unsupported.
    async fn apply_bucket_config_at(
        &self,
        _bucket: &str,
        _config_key: &str,
        _config_value: &str,
        _updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "apply_bucket_config_at: cluster replication is not supported by this backend"
                .to_string(),
        ))
    }

    /// Replaces a bucket's whole tag set preserving the given `updated_at`
    /// verbatim (R5/TD-016) — unlike [`MetadataStore::put_bucket_tags`], which
    /// stamps `now()`.
    ///
    /// Default implementation: unsupported.
    async fn apply_bucket_tags_at(
        &self,
        _bucket: &str,
        _tags: &[(String, String)],
        _updated_at: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "apply_bucket_tags_at: cluster replication is not supported by this backend"
                .to_string(),
        ))
    }

    /// Returns object rows whose node-local `seq` is strictly greater than
    /// `since`, ordered by ascending `seq`, capped at `limit`, each paired with
    /// its `seq`. This is the cluster anti-entropy changed-since cursor: a peer
    /// tracks the highest `seq` it has applied from this node and asks for
    /// everything newer.
    ///
    /// `seq` is a per-node monotonic write counter stamped on every local write
    /// (including [`MetadataStore::apply_remote_object`], so reconciliation
    /// propagates transitively A→B→C). It reflects this node's write order and
    /// is immune to wall-clock skew, unlike `last_modified` (which is the
    /// object's replicated logical mtime, not a per-node write order). Hard
    /// deletes are NOT surfaced here — the row is gone — so deletes propagate
    /// via real-time fan-out and hinted-handoff, not the manifest.
    ///
    /// Default implementation: unsupported (for non-clustered backends).
    async fn list_rows_changed_since(
        &self,
        _since: u64,
        _limit: u32,
    ) -> Result<Vec<(u64, ObjectRecord)>, crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "list_rows_changed_since: cluster replication is not supported by this backend"
                .to_string(),
        ))
    }

    /// The highest node-local object `seq` assigned so far (the current value
    /// of the write counter), `0` when no write has ever taken one. Reported
    /// by the authenticated cluster ping so a peer can detect a seq REWIND
    /// (this node restored from an older backup while the peer's high-water
    /// mark still points past it — D3c). Reads the counter, not `MAX(seq)`
    /// over rows: purged tombstones make the row maximum go backwards, which
    /// would false-alarm the rewind detection.
    ///
    /// Default implementation: `0` (for non-clustered backends).
    async fn current_object_seq(&self) -> Result<u64, crate::error::ArcaError> {
        Ok(0)
    }

    /// Reconciles the per-node `object_seq` write counter to at least
    /// `MAX(seq)` over the objects table, returning the value the counter now
    /// holds. Used by `arca migrate-topology --to-cluster` to make a standalone
    /// instance the consistent first node of a cluster: the next clustered
    /// write is then guaranteed a strictly-larger seq than any pre-cluster row,
    /// so a peer's changed-since cursor never skips this node's existing
    /// objects. The counter is only ever bumped UP, never rewound (a rewind
    /// would false-alarm peer D3c rewind detection), so this is idempotent and
    /// safe even when the counter is already consistent (the normal case, since
    /// every local write advances it) — it repairs counters left behind by an
    /// offline `recover`/`migrate-db` that rebuilt rows out of band.
    ///
    /// Default implementation: no-op (`Ok(0)`) — backends without the counter.
    async fn seed_object_seq_to_max(&self) -> Result<u64, crate::error::ArcaError> {
        Ok(0)
    }

    /// Removes tombstone rows (hard-deleted versions kept only for cluster
    /// convergence) whose `last_modified` is older than `before`, returning the
    /// number removed. The grace period (`now - before`) MUST exceed the longest
    /// expected node downtime: a returning node still needs the tombstone to
    /// learn of the deletion, otherwise anti-entropy would resurrect the object.
    ///
    /// Default implementation: no-op (`Ok(0)`) — single-node deployments never
    /// create tombstones (hard deletes remove the row outright).
    async fn purge_tombstones(
        &self,
        _before: chrono::DateTime<chrono::Utc>,
    ) -> Result<u64, crate::error::ArcaError> {
        Ok(0)
    }

    /// Returns every `blob_id` currently referenced by metadata: live object
    /// rows (non-tombstone, non-empty `blob_id`) plus in-progress multipart
    /// `parts`. This is the metadata side of the cluster blob repair/GC scan —
    /// repair fetches missing bytes for these, and GC treats any on-disk blob
    /// NOT in this set (nor in a composite sidecar) as a reclaim candidate.
    /// Composite-completed multipart parts are referenced by the composite
    /// sidecar, NOT here, so the GC caller must union those in (data-loss guard).
    ///
    /// Default implementation: unsupported (for non-clustered backends).
    async fn list_referenced_blob_ids(
        &self,
    ) -> Result<Vec<crate::types::BlobId>, crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "list_referenced_blob_ids: not supported by this backend".to_string(),
        ))
    }
}

#[cfg(test)]
mod precondition_tests {
    use super::*;
    use crate::types::BlobId;
    use std::collections::HashMap;

    fn make_record(etag: &str) -> ObjectRecord {
        ObjectRecord {
            bucket: "b".to_string(),
            key: "k".to_string(),
            blob_id: BlobId("blob".to_string()),
            size: 100,
            etag: etag.to_string(),
            content_type: None,
            last_modified: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            metadata: HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            replication_status: None,
            lock_updated_at: None,
            content_updated_at: None,
        }
    }

    // -- WritePrecondition --

    #[test]
    fn write_precondition_empty_always_ok() {
        let pre = WritePrecondition::default();
        assert!(pre.is_empty());
        assert!(pre.evaluate(None).is_ok());
        assert!(pre.evaluate(Some(&make_record("abc"))).is_ok());
    }

    #[test]
    fn write_if_match_star_requires_existing_object() {
        let pre = WritePrecondition {
            if_match: Some("*".to_string()),
            if_none_match: None,
        };
        assert!(pre.evaluate(Some(&make_record("abc"))).is_ok());
        assert_eq!(pre.evaluate(None), Err(S3ErrorCode::NoSuchKey));
    }

    #[test]
    fn write_if_match_specific_etag() {
        let pre = WritePrecondition {
            if_match: Some("\"abc\"".to_string()),
            if_none_match: None,
        };
        assert!(pre.evaluate(Some(&make_record("abc"))).is_ok());
        assert_eq!(
            pre.evaluate(Some(&make_record("other"))),
            Err(S3ErrorCode::PreconditionFailed)
        );
        assert_eq!(pre.evaluate(None), Err(S3ErrorCode::NoSuchKey));
    }

    #[test]
    fn write_if_match_comma_separated_list() {
        let pre = WritePrecondition {
            if_match: Some("\"foo\", \"abc\", \"bar\"".to_string()),
            if_none_match: None,
        };
        assert!(pre.evaluate(Some(&make_record("abc"))).is_ok());
        assert_eq!(
            pre.evaluate(Some(&make_record("zzz"))),
            Err(S3ErrorCode::PreconditionFailed)
        );
    }

    #[test]
    fn write_if_none_match_star_requires_absence() {
        let pre = WritePrecondition {
            if_match: None,
            if_none_match: Some("*".to_string()),
        };
        assert!(pre.evaluate(None).is_ok());
        assert_eq!(
            pre.evaluate(Some(&make_record("abc"))),
            Err(S3ErrorCode::PreconditionFailed)
        );
    }

    #[test]
    fn write_if_none_match_specific_etag_refused_on_match_allowed_otherwise() {
        // AWS documents only `*` for PutObject's If-None-Match, but Arca keeps
        // standard HTTP semantics for a specific ETag (see plan §3.5).
        let pre = WritePrecondition {
            if_match: None,
            if_none_match: Some("\"abc\"".to_string()),
        };
        assert_eq!(
            pre.evaluate(Some(&make_record("abc"))),
            Err(S3ErrorCode::PreconditionFailed)
        );
        assert!(pre.evaluate(Some(&make_record("other"))).is_ok());
        assert!(pre.evaluate(None).is_ok());
    }

    // -- DeletePrecondition --

    #[test]
    fn delete_precondition_empty_always_ok() {
        let pre = DeletePrecondition::default();
        assert!(pre.is_empty());
        assert!(pre.evaluate(None).is_ok());
        assert!(pre.evaluate(Some(&make_record("abc"))).is_ok());
    }

    #[test]
    fn delete_precondition_absent_object_is_always_ok() {
        // DeleteObject on a missing key is a no-op, never a precondition failure.
        let pre = DeletePrecondition {
            if_match: Some("\"abc\"".to_string()),
            if_match_last_modified: None,
            if_match_size: None,
        };
        assert!(pre.evaluate(None).is_ok());
    }

    #[test]
    fn delete_if_match_etag_mismatch_refuses() {
        let pre = DeletePrecondition {
            if_match: Some("\"abc\"".to_string()),
            if_match_last_modified: None,
            if_match_size: None,
        };
        assert!(pre.evaluate(Some(&make_record("abc"))).is_ok());
        assert_eq!(
            pre.evaluate(Some(&make_record("other"))),
            Err(S3ErrorCode::PreconditionFailed)
        );
    }

    #[test]
    fn delete_if_match_size_mismatch_refuses() {
        let mut rec = make_record("abc");
        rec.size = 42;
        let pre = DeletePrecondition {
            if_match: None,
            if_match_last_modified: None,
            if_match_size: Some(42),
        };
        assert!(pre.evaluate(Some(&rec)).is_ok());
        rec.size = 7;
        assert_eq!(pre.evaluate(Some(&rec)), Err(S3ErrorCode::PreconditionFailed));
    }

    #[test]
    fn delete_if_match_last_modified_mismatch_refuses() {
        let rec = make_record("abc");
        let pre = DeletePrecondition {
            if_match: None,
            if_match_last_modified: Some(rec.last_modified),
            if_match_size: None,
        };
        assert!(pre.evaluate(Some(&rec)).is_ok());
        let pre_mismatch = DeletePrecondition {
            if_match: None,
            if_match_last_modified: Some(rec.last_modified + chrono::Duration::seconds(1)),
            if_match_size: None,
        };
        assert_eq!(
            pre_mismatch.evaluate(Some(&rec)),
            Err(S3ErrorCode::PreconditionFailed)
        );
    }

    #[test]
    fn delete_precondition_against_delete_marker_as_latest() {
        // A delete marker's etag is empty — If-Match against it fails unless
        // the client (unusually) matches the empty ETag.
        let mut dm = make_record("");
        dm.is_delete_marker = true;
        let pre = DeletePrecondition {
            if_match: Some("\"abc\"".to_string()),
            if_match_last_modified: None,
            if_match_size: None,
        };
        assert_eq!(pre.evaluate(Some(&dm)), Err(S3ErrorCode::PreconditionFailed));
    }
}
