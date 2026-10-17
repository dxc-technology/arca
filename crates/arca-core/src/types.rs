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

    /// Whether this is exactly the form [`BlobId::new`] mints (a lowercase
    /// hyphenated UUID). Ids from outside the process (cluster peers, sidecars
    /// read off disk) must pass this before they become a file name.
    pub fn is_valid(&self) -> bool {
        uuid::Uuid::parse_str(&self.0).is_ok_and(|u| u.hyphenated().to_string() == self.0)
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

impl ObjectRecord {
    /// Whether the blob this row references is acceptable: none (delete
    /// markers and tombstones carry an empty id) or a valid [`BlobId`].
    pub fn has_valid_blob_ref(&self) -> bool {
        self.blob_id.0.is_empty() || self.blob_id.is_valid()
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
    /// Base64-encoded checksum value. For a multipart object this is the
    /// COMPOSITE value (`<base64>-<parts>`) or the FULL_OBJECT CRC, see
    /// `handlers::multipart_checksum`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_value: Option<String>,
    /// Layout of an object assembled by CompleteMultipartUpload: its parts in
    /// order, with their sizes and checksums. `None` for any other object and
    /// for multipart objects completed before the layout was recorded.
    ///
    /// Filled when a record is built to be written or replicated, and by the
    /// replication manifest; NOT by the reads that serve S3 requests (it can be
    /// 10,000 entries): those use `MetadataStore::get_object_parts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parts: Option<Vec<ObjectPart>>,
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
    /// (lock-state ordering, since v0.26.0). `Option` ordering (`None < Some`) makes a
    /// row that ever saw a lock change beat one that never did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_updated_at: Option<DateTime<Utc>>,
    /// When the physical content of this row (its `blob_id` + encryption
    /// columns) last changed in place WITHOUT bumping `last_modified` — i.e. a
    /// copy-on-write re-encryption (Phase 30). This is a SECOND in-place
    /// dimension, independent of `lock_updated_at`: re-encryption and an Object
    /// Lock change touch disjoint column groups, so collapsing both onto
    /// `lock_updated_at` let a re-encryption's fresh timestamp clobber a peer's
    /// newer lock state (dropping a legal hold — a WORM violation) and vice
    /// versa. `apply_remote_object` therefore merges the two groups separately
    /// on a `last_modified` tie: lock columns follow `lock_updated_at`, content
    /// columns follow `content_updated_at`. `Option` ordering (`None < Some`)
    /// makes a re-encrypted row beat one that was never re-encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_updated_at: Option<DateTime<Utc>>,
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
    /// (identical-row redelivery no-op): without the skip, two caught-up nodes redeliver their
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

    /// Resolves an incoming replicated record (`self`) against the local row
    /// (`local`) for the same (bucket, key, version). Returns the row to
    /// persist, or `None` to keep the local row unchanged (the incoming record
    /// lost the LWW, or is identical → skip the rewrite so two caught-up nodes
    /// don't redeliver forever).
    ///
    /// `last_modified` is the primary clock: a new PUT bumps it and replaces the
    /// whole row. On a `last_modified` TIE the row carries two INDEPENDENT
    /// in-place registers that are merged separately rather than picking one
    /// whole row:
    /// - the **lock** register (`retention_mode`, `retain_until_date`,
    ///   `legal_hold_status`) ordered by `lock_updated_at`;
    /// - the **content** register (`blob_id`, `encryption_algorithm`,
    ///   `encryption_key_id`) ordered by `content_updated_at`.
    ///
    /// Merging them independently is what keeps a copy-on-write re-encryption
    /// (which bumps `content_updated_at`) from clobbering a peer's newer lock
    /// state (which bumps `lock_updated_at`) and vice versa — collapsing both
    /// onto one dimension dropped legal holds (a WORM violation) and silently
    /// reverted re-encryptions. A genuine concurrent overwrite (same
    /// `last_modified` but DIFFERENT content, i.e. different `etag`) is not a
    /// shared lineage, so it is resolved whole-row by a deterministic `blob_id`
    /// tiebreak, preserving the pre-existing null-version LWW-register behavior.
    pub fn resolve_replicated(&self, local: &ObjectRecord) -> Option<ObjectRecord> {
        use std::cmp::Ordering;
        match self.last_modified.cmp(&local.last_modified) {
            // Strictly newer PUT replaces everything (including resetting the
            // lock/content registers to whatever the new version carries).
            Ordering::Greater => Some(self.clone()),
            // Strictly older PUT loses.
            Ordering::Less => None,
            Ordering::Equal => {
                if self.etag != local.etag {
                    // Concurrent overwrite of different content that happens to
                    // share a timestamp: deterministic whole-row tiebreak.
                    if self.blob_id.0 > local.blob_id.0 {
                        Some(self.clone())
                    } else {
                        None
                    }
                } else {
                    // Same content lineage: merge the two in-place registers.
                    // Each adopts the incoming value only when STRICTLY newer, so
                    // a tie keeps the local row (idempotent redelivery).
                    let mut merged = local.clone();
                    if self.lock_updated_at > local.lock_updated_at {
                        merged.retention_mode = self.retention_mode.clone();
                        merged.retain_until_date = self.retain_until_date;
                        merged.legal_hold_status = self.legal_hold_status.clone();
                        merged.lock_updated_at = self.lock_updated_at;
                    }
                    if self.content_updated_at > local.content_updated_at {
                        merged.blob_id = self.blob_id.clone();
                        merged.encryption_algorithm = self.encryption_algorithm.clone();
                        merged.encryption_key_id = self.encryption_key_id.clone();
                        merged.content_updated_at = self.content_updated_at;
                    }
                    if merged.same_replicated_content(local) {
                        None
                    } else {
                        Some(merged)
                    }
                }
            }
        }
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

/// One part of an object assembled by CompleteMultipartUpload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectPart {
    pub part_number: u32,
    pub size: u64,
    /// Base64 checksum of the part, in the algorithm of the upload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
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
    /// Checksum type selected at CreateMultipartUpload time (`COMPOSITE` or
    /// `FULL_OBJECT`); `None` means the default of the algorithm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum_type: Option<String>,
    /// Username of the user that initiated the upload (reported as `Owner` and
    /// `Initiator` by ListParts and ListMultipartUploads). Defaults to `root`
    /// for records written before the field existed.
    #[serde(default = "default_initiator")]
    pub initiator: String,
}

fn default_initiator() -> String {
    "root".to_string()
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
    /// The user that owns this credential.
    ///
    /// Privileges are a property of this user, never of the credential: root
    /// users have implicit full access, everyone else is authorized through
    /// their grants. This type is serialized on the cluster control plane; a
    /// pre-0.29 `admin` field from an older peer is ignored rather than
    /// rejected (serde skips unknown fields).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blob_id_is_valid_accepts_only_minted_form() {
        assert!(BlobId::new().is_valid());
        assert!(BlobId("123e4567-e89b-42d3-a456-426614174000".into()).is_valid());
        for bad in [
            "",
            "..",
            "../../etc/passwd",
            "a/b",
            "ab",
            "\u{e9}\u{e9}\u{e9}",
            "blob-1",
            // Valid UUIDs in a form this codebase never mints.
            "123E4567-E89B-42D3-A456-426614174000",
            "123e4567e89b42d3a456426614174000",
            "{123e4567-e89b-42d3-a456-426614174000}",
            "urn:uuid:123e4567-e89b-42d3-a456-426614174000",
        ] {
            assert!(!BlobId(bad.into()).is_valid(), "{bad:?} must be invalid");
        }
    }

    fn credential() -> Credential {
        Credential {
            access_key_id: "AKIATEST".to_string(),
            secret_access_key: "secret".to_string(),
            description: "test".to_string(),
            created_at: Utc::now(),
            active: true,
            user_id: "root".to_string(),
        }
    }

    /// The flag is gone from the wire format too: privileges are read from the
    /// owning user, so the owner is what a peer needs.
    #[test]
    fn credential_serializes_its_owner_and_no_admin_flag() {
        let json = serde_json::to_value(credential()).unwrap();
        assert_eq!(json["user_id"], serde_json::json!("root"));
        assert!(json.get("admin").is_none());
    }

    /// The reverse direction: an `admin` field from an older peer is ignored
    /// rather than rejected.
    #[test]
    fn credential_ignores_an_incoming_admin_field() {
        let json = serde_json::json!({
            "access_key_id": "AKIAOLD",
            "secret_access_key": "secret",
            "description": "from an older peer",
            "created_at": "2026-01-01T00:00:00Z",
            "active": true,
            "admin": false,
            "user_id": "root",
        });
        let cred: Credential = serde_json::from_value(json).unwrap();
        assert_eq!(cred.access_key_id, "AKIAOLD");
        assert_eq!(cred.user_id, "root");
    }
}

#[cfg(test)]
mod serde_default_tests {
    use super::*;

    #[test]
    fn multipart_record_without_initiator_defaults_to_root() {
        // A record replicated by a node older than the initiator field.
        let json = r#"{"upload_id":"u","bucket":"b","key":"k","content_type":null,
            "initiated_at":"2026-01-01T00:00:00Z"}"#;
        let record: MultipartUploadRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.initiator, "root");
    }

    #[test]
    fn object_record_without_a_layout_defaults_to_none() {
        // A row replicated by a node older than the part layout.
        let json = r#"{"bucket":"b","key":"k","blob_id":"00000000-0000-4000-8000-000000000000","size":1,
            "etag":"e","content_type":null,"last_modified":"2026-01-01T00:00:00Z"}"#;
        let record: ObjectRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.parts, None);
    }

    #[test]
    fn object_part_round_trips_and_omits_an_absent_checksum() {
        let part = ObjectPart { part_number: 3, size: 7, checksum: None };
        let json = serde_json::to_string(&part).unwrap();
        assert_eq!(json, r#"{"part_number":3,"size":7}"#);
        assert_eq!(serde_json::from_str::<ObjectPart>(&json).unwrap(), part);
    }

    #[test]
    fn multipart_record_without_a_checksum_type_defaults_to_none() {
        let json = r#"{"upload_id":"u","bucket":"b","key":"k","content_type":null,
            "initiated_at":"2026-01-01T00:00:00Z"}"#;
        let record: MultipartUploadRecord = serde_json::from_str(json).unwrap();
        assert_eq!(record.checksum_type, None);
    }
}
