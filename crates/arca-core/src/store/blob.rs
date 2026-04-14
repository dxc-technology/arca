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
///
/// For SSE-S3/SSE-KMS (algorithm="AES256"): all fields present.
/// For SSE-C (algorithm="SSE-C"): only `algorithm` and `nonce_prefix` are set;
/// `encrypted_dek`, `dek_nonce`, and `key_id` are empty (customer manages key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobEncryptionInfo {
    /// Algorithm identifier: "AES256" for SSE-S3/SSE-KMS, "SSE-C" for SSE-C.
    pub algorithm: String,
    /// Base64-encoded encrypted DEK (data encryption key). Empty for SSE-C.
    #[serde(default)]
    pub encrypted_dek: String,
    /// Base64-encoded nonce used to wrap the DEK. Empty for SSE-C.
    #[serde(default)]
    pub dek_nonce: String,
    /// Base64-encoded 4-byte random nonce prefix for chunk encryption.
    pub nonce_prefix: String,
    /// Key ID: first 8 hex chars of SHA-256(master_key). Empty for SSE-C.
    #[serde(default)]
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
    /// Version ID for versioned objects. Absent for unversioned (backward compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
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

    /// Concatenates multiple blobs into a single output blob, computing MD5.
    ///
    /// Default implementation reads each blob via `get()` and writes via `put()`.
    /// `FsBlobStore` overrides this with direct file-level concatenation
    /// (no intermediate streams, fewer syscalls).
    async fn concat(
        &self,
        part_blob_ids: &[BlobId],
        output_blob_id: &BlobId,
    ) -> Result<BlobPutResult, crate::error::ArcaError> {
        let mut combined: ByteStream = Box::pin(tokio_stream::empty());
        for blob_id in part_blob_ids {
            let result = self.get(blob_id, None).await?;
            combined = Box::pin(tokio_stream::StreamExt::chain(combined, result.stream));
        }
        self.put(output_blob_id, combined).await
    }
}

/// Trait for SSE-C (Server-Side Encryption with Customer-provided keys) blob operations.
///
/// Unlike `BlobStore`, the encryption key is per-call (not per-instance).
#[async_trait::async_trait]
pub trait SsecBlobOps: Send + Sync {
    /// Writes a blob encrypted with the customer-provided key.
    /// Returns `(BlobPutResult, nonce_prefix)`.
    async fn put_with_key(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
        customer_key: &[u8; 32],
    ) -> Result<(BlobPutResult, [u8; 4]), crate::error::ArcaError>;

    /// Reads a blob encrypted with SSE-C using the customer-provided key.
    async fn get_with_key(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
        customer_key: &[u8; 32],
        nonce_prefix: &[u8; 4],
        plaintext_size: u64,
    ) -> Result<BlobGetResult, crate::error::ArcaError>;

    /// Deletes a blob (no key needed).
    async fn delete(&self, blob_id: &BlobId) -> Result<(), crate::error::ArcaError>;

    /// Writes sidecar metadata alongside the blob file.
    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), crate::error::ArcaError>;
}
