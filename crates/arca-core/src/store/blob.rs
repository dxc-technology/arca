//! Blob storage trait.

use std::io;
use std::pin::Pin;

use bytes::Bytes;
use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::types::BlobId;

/// A streaming byte source for reading or writing blobs.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

/// Encryption metadata for a blob (stored in sidecar + returned from put).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobEncryptionInfo {
    /// Algorithm identifier (e.g. "AES256").
    pub algorithm: String,
    /// Base64-encoded encrypted DEK (data encryption key).
    pub encrypted_dek: String,
    /// Base64-encoded nonce used to wrap the DEK.
    pub dek_nonce: String,
    /// Base64-encoded 4-byte random nonce prefix for chunk encryption.
    pub nonce_prefix: String,
    /// Key ID: first 8 hex chars of SHA-256(master_key), identifies which KEK was used.
    pub key_id: String,
}

/// Result of a successful blob put operation.
#[derive(Debug, Clone)]
pub struct BlobPutResult {
    /// Size in bytes of the written blob.
    pub size: u64,
    /// Hex-encoded MD5 hash of the blob content (used as S3 ETag).
    pub etag: String,
    /// Encryption metadata, if the blob was encrypted.
    pub encryption: Option<BlobEncryptionInfo>,
}

/// A byte range for partial reads.
#[derive(Debug, Clone, Copy)]
pub struct ByteRange {
    /// Start byte offset (inclusive).
    pub start: u64,
    /// End byte offset (inclusive). If None, read to end of file.
    pub end: Option<u64>,
}

/// Result of a successful blob get operation.
pub struct BlobGetResult {
    /// Streaming body of the blob (or range thereof).
    pub stream: ByteStream,
    /// Number of bytes in this response (full file or range length).
    pub content_length: u64,
}

/// Sidecar metadata written alongside blob files for disaster recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarMeta {
    pub bucket: String,
    pub key: String,
    pub size: u64,
    pub etag: String,
    pub content_type: Option<String>,
    pub last_modified: String,
    /// User and system metadata (`x-amz-meta-*`, `cache-control`, etc.).
    /// Defaults to empty for backward compatibility with older sidecar files.
    #[serde(default)]
    pub metadata: std::collections::HashMap<String, String>,
    /// Encryption metadata. Absent/null = unencrypted (backward compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<BlobEncryptionInfo>,
}

/// Trait for blob (binary data) storage operations.
#[async_trait::async_trait]
pub trait BlobStore: Send + Sync {
    /// Writes a blob from a byte stream, computing MD5 as it goes.
    async fn put(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
    ) -> Result<BlobPutResult, crate::error::ArcaError>;

    /// Reads a blob (or byte range) as a stream.
    async fn get(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, crate::error::ArcaError>;

    /// Deletes a blob and its sidecar. Ignores not-found errors.
    async fn delete(&self, blob_id: &BlobId) -> Result<(), crate::error::ArcaError>;

    /// Writes sidecar metadata alongside the blob file.
    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), crate::error::ArcaError>;
}
