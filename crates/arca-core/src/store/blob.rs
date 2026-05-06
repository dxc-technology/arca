//! Blob storage trait.

use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::types::BlobId;

/// Reasons a write bypassed compression, for metrics.
#[derive(Debug, Clone, Copy)]
pub enum CompressionSkipReason {
    Disabled,
    Mime,
    Size,
}

impl CompressionSkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Mime => "mime",
            Self::Size => "size",
        }
    }
}

/// Thread-safe compression metrics shared between the `CompressingBlobStore`
/// (which increments counters) and the Prometheus renderer (which reads them).
#[derive(Debug, Default)]
pub struct CompressionMetrics {
    zstd_plain: AtomicU64,
    zstd_comp: AtomicU64,
    lz4_plain: AtomicU64,
    lz4_comp: AtomicU64,
    snappy_plain: AtomicU64,
    snappy_comp: AtomicU64,
    gzip_plain: AtomicU64,
    gzip_comp: AtomicU64,
    brotli_plain: AtomicU64,
    brotli_comp: AtomicU64,
    xz_plain: AtomicU64,
    xz_comp: AtomicU64,
    skipped_disabled: AtomicU64,
    skipped_mime: AtomicU64,
    skipped_size: AtomicU64,
}

impl CompressionMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_bytes(&self, alg: CompressionAlgorithm, plain: u64, compressed: u64) {
        let (p, c) = match alg {
            CompressionAlgorithm::Zstd => (&self.zstd_plain, &self.zstd_comp),
            CompressionAlgorithm::Lz4 => (&self.lz4_plain, &self.lz4_comp),
            CompressionAlgorithm::Snappy => (&self.snappy_plain, &self.snappy_comp),
            CompressionAlgorithm::Gzip => (&self.gzip_plain, &self.gzip_comp),
            CompressionAlgorithm::Brotli => (&self.brotli_plain, &self.brotli_comp),
            CompressionAlgorithm::Xz => (&self.xz_plain, &self.xz_comp),
        };
        p.fetch_add(plain, Ordering::Relaxed);
        c.fetch_add(compressed, Ordering::Relaxed);
    }

    pub fn record_skipped(&self, reason: CompressionSkipReason) {
        let counter = match reason {
            CompressionSkipReason::Disabled => &self.skipped_disabled,
            CompressionSkipReason::Mime => &self.skipped_mime,
            CompressionSkipReason::Size => &self.skipped_size,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns `(algorithm, plaintext_bytes, compressed_bytes)` tuples for all
    /// algorithms, in a stable order.
    pub fn snapshot_bytes(&self) -> Vec<(CompressionAlgorithm, u64, u64)> {
        vec![
            (
                CompressionAlgorithm::Zstd,
                self.zstd_plain.load(Ordering::Relaxed),
                self.zstd_comp.load(Ordering::Relaxed),
            ),
            (
                CompressionAlgorithm::Lz4,
                self.lz4_plain.load(Ordering::Relaxed),
                self.lz4_comp.load(Ordering::Relaxed),
            ),
            (
                CompressionAlgorithm::Snappy,
                self.snappy_plain.load(Ordering::Relaxed),
                self.snappy_comp.load(Ordering::Relaxed),
            ),
            (
                CompressionAlgorithm::Gzip,
                self.gzip_plain.load(Ordering::Relaxed),
                self.gzip_comp.load(Ordering::Relaxed),
            ),
            (
                CompressionAlgorithm::Brotli,
                self.brotli_plain.load(Ordering::Relaxed),
                self.brotli_comp.load(Ordering::Relaxed),
            ),
            (
                CompressionAlgorithm::Xz,
                self.xz_plain.load(Ordering::Relaxed),
                self.xz_comp.load(Ordering::Relaxed),
            ),
        ]
    }

    /// Returns `(disabled, mime, size)` skip counters.
    pub fn snapshot_skipped(&self) -> (u64, u64, u64) {
        (
            self.skipped_disabled.load(Ordering::Relaxed),
            self.skipped_mime.load(Ordering::Relaxed),
            self.skipped_size.load(Ordering::Relaxed),
        )
    }
}

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

/// Supported compression algorithms for transparent at-rest compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompressionAlgorithm {
    Zstd,
    Lz4,
    Snappy,
    Gzip,
    Brotli,
    Xz,
}

impl CompressionAlgorithm {
    /// Short ASCII name (matches TOML spelling, used in XML and logs).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Zstd => "zstd",
            Self::Lz4 => "lz4",
            Self::Snappy => "snappy",
            Self::Gzip => "gzip",
            Self::Brotli => "brotli",
            Self::Xz => "xz",
        }
    }

    /// Single-byte code embedded in the on-disk frame header.
    pub fn code(&self) -> u8 {
        match self {
            Self::Zstd => 1,
            Self::Lz4 => 2,
            Self::Snappy => 3,
            Self::Gzip => 4,
            Self::Brotli => 5,
            Self::Xz => 6,
        }
    }

    /// Parses a single-byte algorithm code from the on-disk frame header.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Zstd),
            2 => Some(Self::Lz4),
            3 => Some(Self::Snappy),
            4 => Some(Self::Gzip),
            5 => Some(Self::Brotli),
            6 => Some(Self::Xz),
            _ => None,
        }
    }

    /// Parses a TOML/XML string into an algorithm.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "zstd" => Some(Self::Zstd),
            "lz4" => Some(Self::Lz4),
            "snappy" => Some(Self::Snappy),
            "gzip" => Some(Self::Gzip),
            "brotli" => Some(Self::Brotli),
            "xz" => Some(Self::Xz),
            _ => None,
        }
    }
}

/// Compression metadata for a blob (stored in sidecar when a blob was compressed).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlobCompressionInfo {
    /// Concrete algorithm used (never `Auto` — resolved before write).
    pub algorithm: CompressionAlgorithm,
    /// Plaintext chunk size used to frame the file (bytes).
    pub chunk_size: u32,
    /// Original plaintext size in bytes (mirrors `SidecarMeta.size`).
    pub original_size: u64,
    /// On-disk size after compression (framed, including header + footer).
    pub compressed_size: u64,
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
    /// Compression metadata, if the blob was compressed.
    pub compression: Option<BlobCompressionInfo>,
    /// When `Some`, this blob is a composite of already-encrypted parts and
    /// must NOT have a physical file written; the caller is expected to
    /// write a sidecar with `composite: Some(parts)` and skip part cleanup.
    /// Set by `EncryptingBlobStore::concat` to avoid the decrypt+re-encrypt
    /// cost of a default `concat` implementation. `None` for normal blobs.
    pub composite_parts: Option<Vec<CompositePart>>,
}

/// Optional hints provided by the handler at write time, consumed by
/// transparent layers (currently compression). Empty by default — unknown
/// hints fall back to safe defaults inside the wrapper.
#[derive(Debug, Clone, Default)]
pub struct PutHints {
    /// Client-advertised Content-Type (used by compression MIME filter).
    pub content_type: Option<String>,
    /// Expected plaintext size in bytes (used by compression size filter).
    pub size_hint: Option<u64>,
    /// Bucket name, when known, so the wrapper can resolve per-bucket policy.
    pub bucket: Option<String>,
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

/// One part of an encrypted composite blob (the result of an encrypted
/// `CompleteMultipartUpload` that avoids decrypt+re-encrypt).
///
/// When a composite sidecar is read, the parts are streamed and decrypted
/// in order to reconstruct the plaintext. Each part keeps its own DEK
/// and nonce_prefix so concat is a metadata-only operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompositePart {
    /// Blob ID of the on-disk part file (still encrypted, never deleted by
    /// CompleteMultipartUpload — it's now logically part of the composite).
    pub blob_id: BlobId,
    /// Plaintext size in bytes of this part.
    pub plaintext_size: u64,
    /// Hex-encoded MD5 of the plaintext (= the part's S3 ETag).
    pub plaintext_etag: String,
    /// Per-part encryption info: wrapped DEK, nonce prefix, key id.
    pub encryption: BlobEncryptionInfo,
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
    /// Compression metadata. Absent/null = uncompressed (backward compatible).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<BlobCompressionInfo>,
    /// Version ID for versioned objects. Absent for unversioned (backward compat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// Composite parts, when this blob is the result of an encrypted
    /// `CompleteMultipartUpload`. Absent for non-composite blobs (backward
    /// compatible). When present, the on-disk file `{blob_path}` does not
    /// exist; reads stream from each part in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composite: Option<Vec<CompositePart>>,
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

    /// Writes a blob with optional hints (content type, size, bucket) that
    /// transparent wrappers may use to steer their decisions.
    ///
    /// Default implementation ignores the hints and delegates to `put`.
    async fn put_with_hints(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
        _hints: PutHints,
    ) -> Result<BlobPutResult, crate::error::ArcaError> {
        self.put(blob_id, stream).await
    }

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
