//! `BlobStore` wrapper that transparently compresses/decompresses objects.
//!
//! Wraps any inner `BlobStore` (typically `FsBlobStore` alone or
//! `EncryptingBlobStore` stacked on top of `FsBlobStore`) and applies the
//! configured compression algorithm. Compressed and plain blobs coexist
//! transparently (mixed mode): the read path inspects the sidecar to decide
//! whether to pass through or decompress.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use std::collections::HashMap;
use tokio::fs;

use arca_core::error::ArcaError;
use arca_core::store::{
    BlobCompressionInfo, BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream,
    CompressionAlgorithm, CompressionMetrics, CompressionSkipReason, MetadataStore, PutHints,
    SidecarMeta,
};
use arca_core::types::BlobId;

use crate::compression::auto::pick_auto;
use crate::compression::format::{FOOTER_COUNT_SIZE, HEADER_SIZE};
use crate::compression::stream::{
    chunk_disk_offset, decompress_range, CompressingStream, DecompressingStream,
};
use crate::fs::FsBlobStore;

/// Plaintext chunk size used for framing on disk.
pub const DEFAULT_CHUNK_SIZE: u32 = 64 * 1024;

/// Minimum plaintext size below which compression is always skipped.
pub const DEFAULT_MIN_SIZE: u64 = 1024;

/// Content-Type prefixes whose payload is already compressed and should
/// bypass the wrapper even when the bucket has compression configured.
pub const DEFAULT_SKIP_MIME_PREFIXES: &[&str] = &[
    "image/",
    "video/",
    "audio/",
    "application/zip",
    "application/gzip",
    "application/x-7z-compressed",
    "application/x-bzip2",
    "application/x-xz",
    "application/x-rar-compressed",
    "application/pdf",
];

/// Effective per-put compression choice.
#[derive(Debug, Clone)]
struct ResolvedCompression {
    algorithm: CompressionAlgorithm,
    level: i32,
    chunk_size: u32,
}

/// Cached per-bucket compression config, TTL-limited.
#[derive(Default)]
struct BucketCache {
    entries: HashMap<String, (Option<BucketCompressionConfig>, Instant)>,
}

/// Per-bucket compression setting stored as JSON in the `bucket_config`
/// table under key `compression`. Absence of this row = compression disabled.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BucketCompressionConfig {
    /// Algorithm name: "auto" | "zstd" | "lz4" | "snappy" | "gzip" | "brotli" | "xz".
    pub algorithm: String,
    /// Optional compression level; ignored when `algorithm = "auto"`.
    #[serde(default)]
    pub level: Option<i32>,
}

/// Compressing blob store wrapper.
///
/// Compression is **opt-in per bucket** — the wrapper only compresses writes
/// when the target bucket has a `compression` row in `bucket_config`. There
/// is no instance-wide on/off switch; the wrapper is always installed and
/// falls back to passthrough when no bucket configuration is present.
pub struct CompressingBlobStore {
    inner: Arc<dyn BlobStore>,
    fs: Arc<FsBlobStore>,
    metadata: Arc<dyn MetadataStore>,
    bucket_cache: Arc<RwLock<BucketCache>>,
    metrics: Arc<CompressionMetrics>,
}

impl CompressingBlobStore {
    /// Creates a new wrapper. `fs` is the innermost filesystem store, used
    /// only to read sidecars on the read path and to access the on-disk file
    /// for ranged reads. `inner` is the store where the actual writes/reads
    /// go (`fs` directly, or an `EncryptingBlobStore` wrapping `fs`).
    pub fn new(
        inner: Arc<dyn BlobStore>,
        fs: Arc<FsBlobStore>,
        metadata: Arc<dyn MetadataStore>,
    ) -> Self {
        Self::with_metrics(inner, fs, metadata, Arc::new(CompressionMetrics::new()))
    }

    /// Creates a wrapper that shares an externally-owned metrics counter.
    pub fn with_metrics(
        inner: Arc<dyn BlobStore>,
        fs: Arc<FsBlobStore>,
        metadata: Arc<dyn MetadataStore>,
        metrics: Arc<CompressionMetrics>,
    ) -> Self {
        Self {
            inner,
            fs,
            metadata,
            bucket_cache: Arc::new(RwLock::new(BucketCache::default())),
            metrics,
        }
    }

    /// Returns a handle to the compression metrics counter.
    pub fn metrics(&self) -> Arc<CompressionMetrics> {
        self.metrics.clone()
    }

    /// Invalidate the cached per-bucket compression config for `bucket`.
    pub fn invalidate_bucket(&self, bucket: &str) {
        if let Ok(mut cache) = self.bucket_cache.write() {
            cache.entries.remove(bucket);
        }
    }

    async fn load_bucket_config(&self, bucket: &str) -> Option<BucketCompressionConfig> {
        let now = Instant::now();
        if let Ok(cache) = self.bucket_cache.read() {
            if let Some((val, expires)) = cache.entries.get(bucket) {
                if *expires > now {
                    return val.clone();
                }
            }
        }
        let val = match self.metadata.get_bucket_config(bucket, "compression").await {
            Ok(Some(raw)) => serde_json::from_str::<BucketCompressionConfig>(&raw).ok(),
            _ => None,
        };
        if let Ok(mut cache) = self.bucket_cache.write() {
            cache
                .entries
                .insert(bucket.to_string(), (val.clone(), now + Duration::from_secs(30)));
        }
        val
    }

    /// Resolve the effective compression choice for a put, or `None` if
    /// the write should pass through uncompressed. Skip reasons are recorded
    /// in metrics.
    async fn resolve(&self, hints: &PutHints) -> Option<ResolvedCompression> {
        // Compression is strictly opt-in per bucket.
        let bucket_cfg = match &hints.bucket {
            Some(b) => self.load_bucket_config(b).await,
            None => {
                self.metrics.record_skipped(CompressionSkipReason::Disabled);
                return None;
            }
        };
        let cfg = match bucket_cfg {
            Some(c) => c,
            None => {
                self.metrics.record_skipped(CompressionSkipReason::Disabled);
                return None;
            }
        };

        // Size filter.
        if let Some(size) = hints.size_hint {
            if size < DEFAULT_MIN_SIZE {
                self.metrics.record_skipped(CompressionSkipReason::Size);
                return None;
            }
        }
        // MIME filter.
        if let Some(ct) = hints.content_type.as_deref() {
            let ct_lower = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
            if DEFAULT_SKIP_MIME_PREFIXES
                .iter()
                .any(|p| ct_lower.starts_with(p))
            {
                self.metrics.record_skipped(CompressionSkipReason::Mime);
                return None;
            }
        }

        let (algorithm, level) = if cfg.algorithm == "auto" {
            pick_auto(hints.content_type.as_deref(), hints.size_hint)
        } else {
            let alg = CompressionAlgorithm::parse(&cfg.algorithm)?;
            (alg, cfg.level.unwrap_or(3))
        };

        Some(ResolvedCompression {
            algorithm,
            level,
            chunk_size: DEFAULT_CHUNK_SIZE,
        })
    }

    async fn read_sidecar(&self, blob_id: &BlobId) -> Result<Option<SidecarMeta>, ArcaError> {
        let p = self.fs.sidecar_path(blob_id);
        match fs::read_to_string(&p).await {
            Ok(json) => {
                let meta: SidecarMeta = serde_json::from_str(&json)
                    .map_err(|e| ArcaError::Internal(format!("parse sidecar: {e}")))?;
                Ok(Some(meta))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ArcaError::Internal(format!("read sidecar: {e}"))),
        }
    }

    async fn get_compressed_full(
        &self,
        blob_id: &BlobId,
        info: &BlobCompressionInfo,
        plaintext_size: u64,
    ) -> Result<BlobGetResult, ArcaError> {
        // Empty plaintext: no chunks to read, yield an empty stream.
        if info.original_size == 0 {
            let _ = plaintext_size;
            let empty: ByteStream = Box::pin(tokio_stream::iter(
                Vec::<Result<Bytes, std::io::Error>>::new(),
            ));
            return Ok(BlobGetResult {
                stream: empty,
                content_length: 0,
            });
        }

        // Read the whole compressed region (sans header and footer) via the
        // inner store so encryption (if any) layers unwrap first.
        let file_size = info.compressed_size;
        let footer_count_offset = file_size.saturating_sub(FOOTER_COUNT_SIZE as u64);
        // Fetch footer count first to know full footer size.
        let count_bytes = self
            .inner
            .get(
                blob_id,
                Some(ByteRange {
                    start: footer_count_offset,
                    end: Some(file_size - 1),
                }),
            )
            .await?;
        let count_buf = collect_stream(count_bytes.stream).await?;
        if count_buf.len() != FOOTER_COUNT_SIZE {
            return Err(ArcaError::Internal("compressed footer count truncated".into()));
        }
        let n = u32::from_le_bytes([count_buf[0], count_buf[1], count_buf[2], count_buf[3]]) as u64;
        let index_bytes = n * 4;
        let footer_start = footer_count_offset - index_bytes;

        if footer_start < HEADER_SIZE as u64 {
            return Err(ArcaError::Internal(
                "compressed footer overlaps header".into(),
            ));
        }

        // Read the data region [HEADER_SIZE .. footer_start).
        let data = self
            .inner
            .get(
                blob_id,
                Some(ByteRange {
                    start: HEADER_SIZE as u64,
                    end: Some(footer_start - 1),
                }),
            )
            .await?;
        let dec_stream = DecompressingStream::new(data.stream, info.algorithm);

        let _ = plaintext_size; // informational; content_length set from sidecar below.
        Ok(BlobGetResult {
            stream: Box::pin(dec_stream),
            content_length: info.original_size,
        })
    }

    async fn get_compressed_range(
        &self,
        blob_id: &BlobId,
        info: &BlobCompressionInfo,
        range: ByteRange,
    ) -> Result<BlobGetResult, ArcaError> {
        let plaintext_size = info.original_size;
        let file_size = info.compressed_size;

        // Read the footer (count + chunk lengths).
        let count_start = file_size - FOOTER_COUNT_SIZE as u64;
        let tail = self
            .inner
            .get(
                blob_id,
                Some(ByteRange {
                    start: count_start,
                    end: Some(file_size - 1),
                }),
            )
            .await?;
        let count_buf = collect_stream(tail.stream).await?;
        let n = u32::from_le_bytes([count_buf[0], count_buf[1], count_buf[2], count_buf[3]]) as usize;

        let index_bytes = (n as u64) * 4;
        let index_start = count_start - index_bytes;

        let index_buf = if n > 0 {
            let got = self
                .inner
                .get(
                    blob_id,
                    Some(ByteRange {
                        start: index_start,
                        end: Some(count_start - 1),
                    }),
                )
                .await?;
            collect_stream(got.stream).await?
        } else {
            Vec::new()
        };

        let mut chunk_lens = Vec::with_capacity(n);
        for i in 0..n {
            let o = i * 4;
            chunk_lens.push(u32::from_le_bytes([
                index_buf[o],
                index_buf[o + 1],
                index_buf[o + 2],
                index_buf[o + 3],
            ]));
        }

        let range_start = range.start;
        let range_end = range
            .end
            .map(|e| e.min(plaintext_size - 1))
            .unwrap_or(plaintext_size - 1);
        let content_length = range_end - range_start + 1;

        let first_chunk = (range_start / info.chunk_size as u64) as usize;
        let last_chunk = (range_end / info.chunk_size as u64) as usize;

        let disk_start = chunk_disk_offset(&chunk_lens, first_chunk);
        // Sum lengths from first_chunk..=last_chunk.
        let mut slab_len: u64 = 0;
        for i in first_chunk..=last_chunk.min(n.saturating_sub(1)) {
            slab_len += chunk_lens[i] as u64;
        }
        let disk_end = disk_start + slab_len;

        let data = self
            .inner
            .get(
                blob_id,
                Some(ByteRange {
                    start: disk_start,
                    end: Some(disk_end - 1),
                }),
            )
            .await?;
        let data_buf = collect_stream(data.stream).await?;

        let plaintext = decompress_range(
            info.algorithm,
            info.chunk_size,
            &data_buf,
            first_chunk as u64,
            range_start,
            range_end,
        )
        .map_err(|e| ArcaError::Internal(format!("decompress range: {e}")))?;

        let stream: ByteStream = Box::pin(tokio_stream::iter(vec![Ok(Bytes::from(plaintext))]));
        Ok(BlobGetResult {
            stream,
            content_length,
        })
    }
}

async fn collect_stream(stream: ByteStream) -> Result<Vec<u8>, ArcaError> {
    use tokio_stream::StreamExt;
    let mut s = std::pin::pin!(stream);
    let mut buf = Vec::new();
    while let Some(chunk) = s.as_mut().next().await {
        let chunk = chunk.map_err(|e| ArcaError::Internal(format!("read stream: {e}")))?;
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

#[async_trait::async_trait]
impl BlobStore for CompressingBlobStore {
    async fn put(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
    ) -> Result<BlobPutResult, ArcaError> {
        // Without hints, we cannot honor MIME/size filters cleanly — fall back
        // to passthrough to preserve existing behavior for callers that
        // haven't been updated.
        self.inner.put(blob_id, stream).await
    }

    async fn put_with_hints(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
        hints: PutHints,
    ) -> Result<BlobPutResult, ArcaError> {
        let resolved = match self.resolve(&hints).await {
            Some(r) => r,
            None => {
                return self.inner.put_with_hints(blob_id, stream, hints).await;
            }
        };

        let (comp_stream, plaintext_stats) = CompressingStream::new(
            stream,
            resolved.algorithm,
            resolved.level,
            resolved.chunk_size,
        );

        // Delegate to inner (which may be EncryptingBlobStore).
        let inner_result = self
            .inner
            .put_with_hints(blob_id, Box::pin(comp_stream), hints.clone())
            .await?;

        let stats = plaintext_stats.lock().unwrap();
        let md5_bytes = stats
            .md5
            .ok_or_else(|| ArcaError::Internal("plaintext MD5 not computed".into()))?;

        // inner_result.size reflects the compressed (+encrypted) on-disk size
        // of what we fed to inner. For plain inner (FsBlobStore), this equals
        // the compressed framed size. For EncryptingBlobStore, it's the
        // plaintext-to-encryption size (i.e. the compressed stream length).
        let compressed_size = inner_result.size;
        self.metrics
            .record_bytes(resolved.algorithm, stats.size, compressed_size);

        Ok(BlobPutResult {
            size: stats.size,
            etag: hex::encode(md5_bytes),
            encryption: inner_result.encryption,
            compression: Some(BlobCompressionInfo {
                algorithm: resolved.algorithm,
                chunk_size: resolved.chunk_size,
                original_size: stats.size,
                compressed_size,
            }),
        })
    }

    async fn get(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        let sidecar = self.read_sidecar(blob_id).await?;
        let comp_info = sidecar.as_ref().and_then(|s| s.compression.as_ref());
        match comp_info {
            None => self.inner.get(blob_id, range).await,
            Some(info) => match range {
                None => {
                    let plaintext_size = sidecar
                        .as_ref()
                        .map(|s| s.size)
                        .unwrap_or(info.original_size);
                    self.get_compressed_full(blob_id, info, plaintext_size).await
                }
                Some(range) => self.get_compressed_range(blob_id, info, range).await,
            },
        }
    }

    async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        self.inner.delete(blob_id).await
    }

    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), ArcaError> {
        self.inner.write_sidecar(blob_id, meta).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteStore;
    use std::collections::HashMap;
    use tokio_stream::iter as stream_iter;
    use tokio_stream::StreamExt;

    fn bytes_stream(data: &[u8]) -> ByteStream {
        Box::pin(stream_iter(vec![Ok(Bytes::copy_from_slice(data))]))
    }

    async fn collect(stream: ByteStream) -> Vec<u8> {
        let mut s = std::pin::pin!(stream);
        let mut out = Vec::new();
        while let Some(c) = s.as_mut().next().await {
            out.extend_from_slice(&c.unwrap());
        }
        out
    }

    /// Build a `CompressingBlobStore` with an in-memory SQLite, a temp blobs
    /// dir, and (optionally) a per-bucket compression config for `bucket`.
    async fn make_store(
        bucket: &str,
        algorithm: Option<&str>,
    ) -> (CompressingBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let fs = Arc::new(FsBlobStore::new(dir.path().join("blobs"), 2).await.unwrap());
        let sqlite = Arc::new(SqliteStore::open_in_memory().await.unwrap());
        let metadata: Arc<dyn MetadataStore> = sqlite.clone();
        // Seed bucket + optional compression config.
        metadata.create_bucket(bucket).await.unwrap();
        if let Some(alg) = algorithm {
            let cfg = BucketCompressionConfig {
                algorithm: alg.to_string(),
                level: None,
            };
            metadata
                .set_bucket_config(bucket, "compression", &serde_json::to_string(&cfg).unwrap())
                .await
                .unwrap();
        }
        let store = CompressingBlobStore::new(
            fs.clone() as Arc<dyn BlobStore>,
            fs,
            metadata,
        );
        (store, dir)
    }

    #[tokio::test]
    async fn roundtrip_zstd_full_read() {
        let (store, _dir) = make_store("b", Some("zstd")).await;
        let blob_id = BlobId::new();
        // Must exceed DEFAULT_MIN_SIZE (1024) to bypass the size filter.
        let data: Vec<u8> = b"Lorem ipsum dolor sit amet. ".repeat(100);
        let data = data.as_slice();

        let result = store
            .put_with_hints(
                &blob_id,
                bytes_stream(data),
                PutHints {
                    content_type: Some("text/plain".into()),
                    size_hint: Some(data.len() as u64),
                    bucket: Some("b".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(result.size, data.len() as u64);
        assert!(result.compression.is_some());

        // Sidecar must be written for read path to know it's compressed.
        let sidecar = SidecarMeta {
            bucket: "b".into(),
            key: "k".into(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: Some("text/plain".into()),
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            compression: result.compression.clone(),
            version_id: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        let got = store.get(&blob_id, None).await.unwrap();
        assert_eq!(got.content_length, data.len() as u64);
        assert_eq!(collect(got.stream).await, data);
    }

    #[tokio::test]
    async fn mime_skip_passthrough() {
        let (store, _dir) = make_store("b", Some("zstd")).await;
        let blob_id = BlobId::new();
        // Above DEFAULT_MIN_SIZE (1024) so the size filter doesn't mask the MIME skip.
        let data: Vec<u8> = b"fake png content ".repeat(100);
        let data = data.as_slice();

        let result = store
            .put_with_hints(
                &blob_id,
                bytes_stream(data),
                PutHints {
                    content_type: Some("image/png".into()),
                    size_hint: Some(data.len() as u64),
                    bucket: Some("b".into()),
                },
            )
            .await
            .unwrap();
        assert!(
            result.compression.is_none(),
            "image/* should bypass compression even when the bucket has it enabled"
        );
    }

    #[tokio::test]
    async fn range_read_after_compression() {
        let (store, _dir) = make_store("b", Some("zstd")).await;
        let blob_id = BlobId::new();
        // Enough data to span multiple on-disk frames (default 64 KiB chunks).
        let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();

        let result = store
            .put_with_hints(
                &blob_id,
                bytes_stream(&data),
                PutHints {
                    content_type: Some("text/plain".into()),
                    size_hint: Some(data.len() as u64),
                    bucket: Some("b".into()),
                },
            )
            .await
            .unwrap();
        let sidecar = SidecarMeta {
            bucket: "b".into(),
            key: "k".into(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: Some("text/plain".into()),
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            compression: result.compression.clone(),
            version_id: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        let got = store
            .get(&blob_id, Some(ByteRange { start: 100, end: Some(80_100) }))
            .await
            .unwrap();
        assert_eq!(got.content_length, 80_001);
        assert_eq!(collect(got.stream).await, data[100..=80_100]);
    }

    #[tokio::test]
    async fn passthrough_when_bucket_has_no_config() {
        // No `Some("zstd")` → no compression config seeded for the bucket.
        let (store, _dir) = make_store("b", None).await;
        let blob_id = BlobId::new();
        let data: Vec<u8> = b"plaintext not compressed ".repeat(100);
        let data = data.as_slice();
        let result = store
            .put_with_hints(
                &blob_id,
                bytes_stream(data),
                PutHints {
                    content_type: Some("text/plain".into()),
                    size_hint: Some(data.len() as u64),
                    bucket: Some("b".into()),
                },
            )
            .await
            .unwrap();
        assert!(result.compression.is_none());
    }
}
