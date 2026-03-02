//! Core domain types for Arca.

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
}

/// Metadata about a stored object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectRecord {
    pub bucket: String,
    pub key: String,
    pub blob_id: BlobId,
    pub size: u64,
    pub etag: String,
    pub content_type: Option<String>,
    pub last_modified: DateTime<Utc>,
}

/// Projection of an object for list responses (lighter than ObjectRecord).
#[derive(Debug, Clone)]
pub struct ListEntry {
    pub key: String,
    pub last_modified: DateTime<Utc>,
    pub etag: String,
    pub size: u64,
    pub storage_class: String,
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
}

/// An S3 access credential (access key + secret key pair).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    pub active: bool,
}
