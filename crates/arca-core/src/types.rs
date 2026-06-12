//! Core domain types for Arca.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Unique identifier for a blob in storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobId(pub String);

impl BlobId {
    /// Generates a new random blob ID (UUID v4).
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for BlobId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BlobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Metadata about a bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BucketInfo {
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// Username of the bucket creator.
    #[serde(default)]
    pub owner: String,
}

/// Metadata about a stored object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectRecord {
    pub bucket: String,
    pub key: String,
    pub blob_id: BlobId,
    pub size: u64,
    pub etag: String,
    pub content_type: Option<String>,
    pub last_modified: DateTime<Utc>,
    /// User metadata (`x-amz-meta-*`) and system metadata headers
    /// (`cache-control`, `content-encoding`, `content-disposition`,
    /// `content-language`, `expires`).
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// Encryption algorithm (e.g. "AES256") if the object is encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_algorithm: Option<String>,
    /// Key ID of the master key used to encrypt this object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key_id: Option<String>,
    /// Username of the object creator.
    #[serde(default)]
    pub owner: String,
    /// Version ID. None for unversioned objects, Some(uuid) for versioned,
    /// Some("null") for suspended-bucket writes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// Whether this is the latest version of the object.
    #[serde(default = "default_true")]
    pub is_latest: bool,
    /// Whether this is a delete marker (has no blob).
    #[serde(default)]
    pub is_delete_marker: bool,
    /// Whether this is a tombstone: a hard-deleted version retained (with no
    /// blob) only so the deletion converges across a cluster and is not
    /// resurrected by anti-entropy. Invisible to all S3 reads, never `is_latest`,
    /// GC'd after a grace period. Always `false` on single-node deployments
    /// (hard deletes there remove the row outright). Phase 29 HA.
    #[serde(default)]
    pub is_tombstone: bool,
    /// Object Lock retention mode: "GOVERNANCE" or "COMPLIANCE".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_mode: Option<String>,
    /// Retain-until-date: object cannot be hard-deleted before this time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_until_date: Option<DateTime<Utc>>,
    /// Legal hold status: "ON" means object cannot be hard-deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legal_hold_status: Option<String>,
    /// Storage class (e.g. "STANDARD", "REDUCED_REDUNDANCY").
    #[serde(default = "default_standard")]
    pub storage_class: String,
    /// Checksum algorithm (e.g. "SHA256", "CRC32", "CRC32C", "CRC64NVME").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_algorithm: Option<String>,
    /// Base64-encoded checksum value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_value: Option<String>,
    /// Replication status: `PENDING`, `COMPLETED`, `FAILED`, or `REPLICA`.
    /// `None` for objects that have never been subject to replication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication_status: Option<String>,
    /// When the Object Lock state (retention/legal hold) of this row last
    /// changed; `None` if never. Lock mutations are the only in-place row
    /// updates that do NOT bump `last_modified` (matching S3:
    /// PutObjectRetention does not change Last-Modified), so the cluster LWW
    /// needs this extra dimension to order two copies of the same version
    /// whose `last_modified` ties: without it a stale lock-free copy
    /// re-applied after a node restart silently clobbers a newer lock state
    /// and the clobber's fresh `seq` propagates the regression cluster-wide
    /// (finding N2, Phase 29.1 R7). `Option` ordering (`None < Some`) makes a
    /// row that ever saw a lock change beat one that never did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_updated_at: Option<DateTime<Utc>>,
}

fn default_standard() -> String {
    "STANDARD".to_string()
}

fn default_true() -> bool {
    true
}

impl ObjectRecord {
    /// True when `other` carries the same replicated content as `self`,
    /// ignoring `is_latest` (derived locally by the apply paths'
    /// `recompute_is_latest`, not a replicated fact — including it would keep
    /// two nodes churning while their version sets transiently differ).
    ///
    /// Used by `apply_remote_object` to skip the rewrite — and the fresh `seq`
    /// it would stamp — when a peer redelivers a row this node already holds
    /// (finding M7): without the skip, two caught-up nodes redeliver their
    /// whole object tables to each other on every anti-entropy pass, forever.
    /// Compares via `PartialEq` on normalized clones so a future field is
    /// included automatically (a missed field would resurface as churn, never
    /// as a missed update).
    pub fn same_replicated_content(&self, other: &ObjectRecord) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        a.is_latest = false;
        b.is_latest = false;
        a == b
    }
}

/// Bucket versioning state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersioningState {
    /// Versioning has never been enabled (default).
    Unversioned,
    /// Versioning is enabled — new PUTs generate version IDs.
    Enabled,
    /// Versioning is suspended — new PUTs get version_id=NULL.
    Suspended,
}

/// Projection of an object for list responses (lighter than ObjectRecord).
#[derive(Debug, Clone)]
pub struct ListEntry {
    pub key: String,
    pub last_modified: DateTime<Utc>,
    pub etag: String,
    pub size: u64,
    pub storage_class: String,
    /// Owner ID (populated when `fetch-owner=true` in ListObjectsV2).
    pub owner_id: Option<String>,
    /// Owner display name (populated when `fetch-owner=true` in ListObjectsV2).
    pub owner_display_name: Option<String>,
}

/// Parameters for building a `ListBucketResult` XML response.
#[derive(Debug)]
pub struct ListBucketResultParams<'a> {
    pub name: &'a str,
    pub prefix: Option<&'a str>,
    pub delimiter: Option<&'a str>,
    pub max_keys: u32,
    pub is_truncated: bool,
    pub key_count: u32,
    pub contents: &'a [ListEntry],
    pub common_prefixes: &'a [String],
    pub continuation_token: Option<&'a str>,
    pub next_continuation_token: Option<&'a str>,
    pub start_after: Option<&'a str>,
    pub encoding_type: Option<&'a str>,
    /// When true, each `<Contents>` entry includes `<Owner>`.
    pub fetch_owner: bool,
}

/// Parameters for building a `ListBucketResult` XML response (V1 format).
#[derive(Debug)]
pub struct ListBucketV1ResultParams<'a> {
    pub name: &'a str,
    pub prefix: Option<&'a str>,
    pub delimiter: Option<&'a str>,
    pub marker: Option<&'a str>,
    pub next_marker: Option<&'a str>,
    pub max_keys: u32,
    pub is_truncated: bool,
    pub contents: &'a [ListEntry],
    pub common_prefixes: &'a [String],
    pub encoding_type: Option<&'a str>,
}

/// Metadata about an in-progress multipart upload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultipartUploadRecord {
    pub upload_id: String,
    pub bucket: String,
    pub key: String,
    pub content_type: Option<String>,
    pub initiated_at: DateTime<Utc>,
    /// User metadata (`x-amz-meta-*`) and system metadata headers,
    /// captured at `CreateMultipartUpload` time and applied to the
    /// final object at `CompleteMultipartUpload`.
    #[serde(default)]
    pub metadata: HashMap<String, String>,
    /// Checksum algorithm selected at CreateMultipartUpload time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_algorithm: Option<String>,
}

/// Metadata about a single uploaded part.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartRecord {
    pub upload_id: String,
    pub part_number: u32,
    pub blob_id: BlobId,
    pub size: u64,
    pub etag: String,
    /// Base64-encoded checksum value for this part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_value: Option<String>,
    /// Timestamp when the part was uploaded (for ListParts response).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<DateTime<Utc>>,
}

/// Aggregate storage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageStats {
    pub bucket_count: u64,
    pub object_count: u64,
    pub total_size_bytes: u64,
}

/// An S3 access credential (access key + secret key pair).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub active: bool,
    /// Deprecated: use policy-based access control instead.
    /// Kept for schema compatibility during migration.
    pub admin: bool,
    /// The user that owns this credential.
    pub user_id: String,
}

/// A named user identity that owns credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub user_id: String,
    pub username: String,
    pub description: String,
    /// Root users have implicit full access to everything.
    pub is_root: bool,
    pub created_at: DateTime<Utc>,
}

/// A team (group) of users that can share grants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub team_id: String,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

/// A named, reusable IAM-compatible policy document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Grant {
    pub grant_id: String,
    pub name: String,
    pub description: String,
    /// The JSON policy document (stored as parsed PolicyDocument).
    pub document: crate::policy::PolicyDocument,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
