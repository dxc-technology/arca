//! `BlobStore` implementation backed by the local filesystem.

use std::io;
use std::path::PathBuf;

use bytes::Bytes;
use futures_core::Stream;
use md5::{Digest, Md5};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;

use arca_core::error::ArcaError;
use arca_core::store::{
    BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream, CompositePart, SidecarMeta,
};
use arca_core::types::BlobId;

/// Filesystem-backed blob store.
///
/// Blobs are stored in a sharded directory hierarchy under `base_dir`.
/// The sharding depth is configurable (1–4 levels, default 2).
///
/// With depth=2 and blob_id `550e8400-e29b-41d4-a716-446655440000`:
///   `base_dir/55/0e/550e8400-e29b-41d4-a716-446655440000`
///
/// Sidecar metadata is stored alongside at `{blob_path}.meta`.
#[derive(Clone)]
pub struct FsBlobStore {
    base_dir: PathBuf,
    prefix_depth: u8,
}

impl FsBlobStore {
    /// Creates a new `FsBlobStore`, ensuring the base directory exists.
    ///
    /// `prefix_depth` controls the number of 2-char prefix directories (1–4, clamped).
    pub async fn new(base_dir: impl Into<PathBuf>, prefix_depth: u8) -> Result<Self, ArcaError> {
        let base_dir = base_dir.into();
        let prefix_depth = prefix_depth.clamp(1, 4);
        fs::create_dir_all(&base_dir)
            .await
            .map_err(|e| ArcaError::Internal(format!("create blobs dir: {e}")))?;
        Ok(Self {
            base_dir,
            prefix_depth,
        })
    }

    /// Computes the full path for a blob ID using the configured prefix depth.
    pub fn blob_path(&self, blob_id: &BlobId) -> PathBuf {
        let id = &blob_id.0;
        // Strip hyphens for prefix extraction (UUID has hyphens at fixed positions).
        let hex_chars: String = id.chars().filter(|c| *c != '-').collect();
        let mut path = self.base_dir.clone();
        for level in 0..self.prefix_depth as usize {
            let start = level * 2;
            let end = (start + 2).min(hex_chars.len());
            path.push(&hex_chars[start..end]);
        }
        path.push(id);
        path
    }

    /// Returns the sidecar metadata path for a blob.
    pub fn sidecar_path(&self, blob_id: &BlobId) -> PathBuf {
        let mut p = self.blob_path(blob_id);
        let mut name = p.file_name().unwrap().to_os_string();
        name.push(".meta");
        p.set_file_name(name);
        p
    }

    /// Returns the temporary file path used during writes.
    fn tmp_path(&self, blob_id: &BlobId) -> PathBuf {
        let mut p = self.blob_path(blob_id);
        let mut name = p.file_name().unwrap().to_os_string();
        name.push(".tmp");
        p.set_file_name(name);
        p
    }

    // ----- Raw access for cluster replication (Phase 29) -----
    //
    // These bypass the composite/encryption/compression logic and operate on
    // the physical on-disk file, so a blob can be replicated byte-for-byte to
    // peers and stored verbatim under the identical `blob_id`. They are inherent
    // methods (not part of `BlobStore`) because the cluster layer always holds a
    // concrete `Arc<FsBlobStore>`, like `CompressingBlobStore` does for sidecars.

    /// Streams the raw on-disk bytes of a blob verbatim (no composite assembly,
    /// no decrypt/decompress). Errors if the physical file is absent (e.g. a
    /// composite blob, which has no file of its own — replicate its parts).
    pub async fn read_raw(&self, blob_id: &BlobId) -> Result<BlobGetResult, ArcaError> {
        let blob_path = self.blob_path(blob_id);
        let file = fs::File::open(&blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("open raw blob: {e}")))?;
        let content_length = file
            .metadata()
            .await
            .map_err(|e| ArcaError::Internal(format!("raw blob metadata: {e}")))?
            .len();
        let stream = ReaderStream::with_capacity(file, 65536);
        Ok(BlobGetResult {
            stream: Box::pin(map_reader_stream(stream)),
            content_length,
        })
    }

    /// Writes raw bytes to a blob verbatim via temp-file + atomic rename, with
    /// no MD5 or sidecar (the caller ships the sidecar separately). Idempotent:
    /// re-delivering the same `blob_id` overwrites with identical bytes.
    pub async fn write_raw(&self, blob_id: &BlobId, stream: ByteStream) -> Result<u64, ArcaError> {
        let blob_path = self.blob_path(blob_id);
        if let Some(parent) = blob_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ArcaError::Internal(format!("create blob dir: {e}")))?;
        }
        let tmp_path = self.tmp_path(blob_id);
        let mut file = fs::File::create(&tmp_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("create raw tmp: {e}")))?;
        let mut size: u64 = 0;
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.as_mut().next().await {
            let chunk = chunk.map_err(|e| ArcaError::Internal(format!("read raw stream: {e}")))?;
            size += chunk.len() as u64;
            file.write_all(&chunk)
                .await
                .map_err(|e| ArcaError::Internal(format!("write raw blob: {e}")))?;
        }
        file.flush()
            .await
            .map_err(|e| ArcaError::Internal(format!("flush raw blob: {e}")))?;
        drop(file);
        fs::rename(&tmp_path, &blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("rename raw blob: {e}")))?;
        Ok(size)
    }

    /// Whether the blob's physical file exists (anti-entropy probe).
    pub async fn exists(&self, blob_id: &BlobId) -> Result<bool, ArcaError> {
        match fs::metadata(self.blob_path(blob_id)).await {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(ArcaError::Internal(format!("stat raw blob: {e}"))),
        }
    }

    /// Lists every physical blob file on disk (excluding sidecars and tmp files),
    /// each with its last-modified time. For the cluster blob GC scan.
    pub async fn list_blob_files(
        &self,
    ) -> Result<Vec<(BlobId, std::time::SystemTime)>, ArcaError> {
        let files = walk_files(self.base_dir.clone()).await?;
        Ok(files
            .into_iter()
            .filter(|(n, _)| !n.ends_with(".meta") && !n.ends_with(".tmp"))
            .map(|(n, t)| (BlobId(n), t))
            .collect())
    }

    /// Lists the blob ids of every sidecar (`.meta`) on disk.
    pub async fn list_sidecar_files(&self) -> Result<Vec<BlobId>, ArcaError> {
        let files = walk_files(self.base_dir.clone()).await?;
        Ok(files
            .into_iter()
            .filter_map(|(n, _)| n.strip_suffix(".meta").map(|s| BlobId(s.to_string())))
            .collect())
    }

    /// Deletes one physical blob file and its sidecar (no composite cascade).
    pub async fn delete_blob_file_raw(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        for path in [self.blob_path(blob_id), self.sidecar_path(blob_id)] {
            match fs::remove_file(&path).await {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(ArcaError::Internal(format!("delete_blob_file: {e}"))),
            }
        }
        Ok(())
    }

    /// Reads and parses the sidecar for a blob. Returns `Ok(None)` if absent.
    async fn read_sidecar(&self, blob_id: &BlobId) -> Result<Option<SidecarMeta>, ArcaError> {
        let path = self.sidecar_path(blob_id);
        match fs::read_to_string(&path).await {
            Ok(json) => {
                let meta: SidecarMeta = serde_json::from_str(&json)
                    .map_err(|e| ArcaError::Internal(format!("parse sidecar: {e}")))?;
                Ok(Some(meta))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ArcaError::Internal(format!("read sidecar: {e}"))),
        }
    }

    /// Streams plaintext from a composite blob (the result of a plain
    /// `CompleteMultipartUpload`). Each part is a normal on-disk blob;
    /// we read only the byte ranges that overlap the request.
    async fn get_composite(
        &self,
        parts: &[CompositePart],
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        let total: u64 = parts.iter().map(|p| p.plaintext_size).sum();
        let (range_start, range_end) = match range {
            Some(r) => {
                let end = r.end.unwrap_or_else(|| total.saturating_sub(1));
                (r.start, end.min(total.saturating_sub(1)))
            }
            None => (0, total.saturating_sub(1)),
        };
        let content_length = if total == 0 {
            0
        } else if range_end >= range_start {
            range_end - range_start + 1
        } else {
            0
        };

        let mut combined: ByteStream = Box::pin(tokio_stream::empty());
        let mut cum_start: u64 = 0;
        for part in parts {
            let part_size = part.plaintext_size;
            let part_end_excl = cum_start + part_size;
            if part_size == 0 || part_end_excl <= range_start || cum_start > range_end {
                cum_start = part_end_excl;
                continue;
            }
            let sub_start = range_start.saturating_sub(cum_start);
            let part_end_incl = part_end_excl - 1;
            let sub_end = if range_end < part_end_incl {
                range_end - cum_start
            } else {
                part_size - 1
            };

            let r = if sub_start == 0 && sub_end == part_size - 1 {
                None
            } else {
                Some(ByteRange {
                    start: sub_start,
                    end: Some(sub_end),
                })
            };
            // Recurse into self.get for the part — the part is a normal blob
            // (no encryption layer at this level), so read_part_file is enough.
            let part_result = self.get_part_data(&part.blob_id, r).await?;
            combined = Box::pin(tokio_stream::StreamExt::chain(combined, part_result.stream));
            cum_start = part_end_excl;
        }

        Ok(BlobGetResult {
            stream: combined,
            content_length,
        })
    }

    /// Reads a part's data file (or sub-range) without going through the
    /// composite-aware top-level `get`. Used internally by `get_composite`.
    async fn get_part_data(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        let blob_path = self.blob_path(blob_id);
        let mut file = fs::File::open(&blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("open part blob: {e}")))?;
        let file_size = file
            .metadata()
            .await
            .map_err(|e| ArcaError::Internal(format!("part metadata: {e}")))?
            .len();

        let content_length = match range {
            Some(ByteRange { start, end }) => {
                file.seek(io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| ArcaError::Internal(format!("seek part: {e}")))?;
                let end = end.map(|e| e.min(file_size - 1)).unwrap_or(file_size - 1);
                end - start + 1
            }
            None => file_size,
        };

        let reader: Box<dyn tokio::io::AsyncRead + Send + Unpin> = match range {
            Some(ByteRange { start: _, end }) => {
                let end = end.map(|e| e.min(file_size - 1)).unwrap_or(file_size - 1);
                let take_bytes = end
                    - file
                        .stream_position()
                        .await
                        .map_err(|e| ArcaError::Internal(format!("stream position: {e}")))?
                    + 1;
                Box::new(file.take(take_bytes))
            }
            None => Box::new(file),
        };

        let stream = ReaderStream::with_capacity(reader, 65536);
        let byte_stream: ByteStream = Box::pin(map_reader_stream(stream));
        Ok(BlobGetResult {
            stream: byte_stream,
            content_length,
        })
    }
}

#[async_trait::async_trait]
impl BlobStore for FsBlobStore {
    async fn put(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
    ) -> Result<BlobPutResult, ArcaError> {
        let blob_path = self.blob_path(blob_id);
        let tmp_path = self.tmp_path(blob_id);

        // Ensure parent directory exists.
        if let Some(parent) = blob_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ArcaError::Internal(format!("create blob dir: {e}")))?;
        }

        // Producer/consumer pipeline. The async task pulls chunks off the
        // ByteStream and forwards them through a bounded channel; a blocking
        // worker owns the file handle, MD5-hashes each chunk and writes it
        // to disk sequentially. Md5::update is CPU-bound and dominated the
        // tokio workers on plain uploads — running it on a blocking thread
        // frees the runtime to keep pulling network bytes. The bound of 4
        // gives the producer enough slack to absorb network jitter while
        // still backpressuring it ahead of the disk.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(4);
        let tmp_for_worker = tmp_path.clone();
        let worker = tokio::task::spawn_blocking(move || -> io::Result<(u64, [u8; 16])> {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp_for_worker)?;
            let mut hasher = Md5::new();
            let mut size: u64 = 0;
            while let Some(chunk) = rx.blocking_recv() {
                hasher.update(&chunk);
                size += chunk.len() as u64;
                file.write_all(&chunk)?;
            }
            file.flush()?;
            let digest: [u8; 16] = hasher.finalize().into();
            Ok((size, digest))
        });

        let mut stream = std::pin::pin!(stream);
        let mut stream_err: Option<ArcaError> = None;
        while let Some(chunk) = stream.as_mut().next().await {
            match chunk {
                Ok(c) => {
                    if tx.send(c).await.is_err() {
                        // Worker exited early (likely io error); the join
                        // below will surface the underlying cause.
                        break;
                    }
                }
                Err(e) => {
                    stream_err = Some(ArcaError::Internal(format!("read stream: {e}")));
                    break;
                }
            }
        }
        drop(tx);

        let (size, digest) = worker
            .await
            .map_err(|e| ArcaError::Internal(format!("blob writer join: {e}")))?
            .map_err(|e| ArcaError::Internal(format!("write blob: {e}")))?;

        if let Some(e) = stream_err {
            return Err(e);
        }

        // Atomic rename: tmp → final.
        fs::rename(&tmp_path, &blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("rename blob: {e}")))?;

        let etag = hex::encode(digest);

        Ok(BlobPutResult { size, etag, encryption: None, compression: None, composite_parts: None })
    }

    async fn get(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        // Composite blob: no on-disk file at this blob_id, parts live
        // separately. Stream from them in order.
        if let Ok(Some(sidecar)) = self.read_sidecar(blob_id).await {
            if let Some(parts) = sidecar.composite.as_ref() {
                return self.get_composite(parts, range).await;
            }
        }
        let blob_path = self.blob_path(blob_id);

        let mut file = fs::File::open(&blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("open blob: {e}")))?;

        let file_size = file
            .metadata()
            .await
            .map_err(|e| ArcaError::Internal(format!("blob metadata: {e}")))?
            .len();

        let content_length = match range {
            Some(ByteRange { start, end }) => {
                file.seek(io::SeekFrom::Start(start))
                    .await
                    .map_err(|e| ArcaError::Internal(format!("seek blob: {e}")))?;
                let end = end.map(|e| e.min(file_size - 1)).unwrap_or(file_size - 1);
                end - start + 1
            }
            None => file_size,
        };

        let reader: Box<dyn tokio::io::AsyncRead + Send + Unpin> = match range {
            Some(ByteRange { start: _, end }) => {
                let end = end.map(|e| e.min(file_size - 1)).unwrap_or(file_size - 1);
                let take_bytes = end - file.stream_position().await.map_err(|e| {
                    ArcaError::Internal(format!("stream position: {e}"))
                })? + 1;
                Box::new(file.take(take_bytes))
            }
            None => Box::new(file),
        };

        let stream = ReaderStream::with_capacity(reader, 65536);
        let byte_stream: ByteStream = Box::pin(map_reader_stream(stream));

        Ok(BlobGetResult {
            stream: byte_stream,
            content_length,
        })
    }

    async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        // Composite blob: cascade-delete every part it points to first.
        // Each part is itself a normal blob (file + sidecar).
        if let Ok(Some(sidecar)) = self.read_sidecar(blob_id).await {
            if let Some(parts) = sidecar.composite {
                for p in &parts {
                    let part_path = self.blob_path(&p.blob_id);
                    let part_sidecar = self.sidecar_path(&p.blob_id);
                    match fs::remove_file(&part_path).await {
                        Ok(()) | Err(_) => {}
                    }
                    match fs::remove_file(&part_sidecar).await {
                        Ok(()) | Err(_) => {}
                    }
                }
            }
        }

        let blob_path = self.blob_path(blob_id);
        let sidecar_path = self.sidecar_path(blob_id);

        // Remove blob file (ignore not found).
        match fs::remove_file(&blob_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(ArcaError::Internal(format!("delete blob: {e}"))),
        }

        // Remove sidecar (ignore not found).
        match fs::remove_file(&sidecar_path).await {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(ArcaError::Internal(format!("delete sidecar: {e}"))),
        }

        Ok(())
    }

    /// Composite-aware concatenation.
    ///
    /// Fast path: when every part is plain (no encryption, no compression),
    /// produce a metadata-only composite — the caller's sidecar will list
    /// the still-on-disk part files. CompleteMultipartUpload becomes O(N)
    /// sidecar reads instead of O(total_size) data copies + MD5.
    ///
    /// Slow path (fallback): when any part has encryption or compression
    /// metadata that this layer can't carry verbatim into a composite,
    /// copy the parts byte-by-byte into a single output file as before.
    async fn concat(
        &self,
        part_blob_ids: &[BlobId],
        output_blob_id: &BlobId,
    ) -> Result<BlobPutResult, ArcaError> {
        // Try the composite fast path first. We need to read every part's
        // sidecar to check for encryption/compression and to capture the
        // per-part etag and size.
        let mut composite_parts: Vec<CompositePart> = Vec::with_capacity(part_blob_ids.len());
        let mut total_size: u64 = 0;
        let mut md5_concat: Vec<u8> = Vec::with_capacity(part_blob_ids.len() * 16);
        let mut needs_fallback = false;

        for blob_id in part_blob_ids {
            let sidecar = match self.read_sidecar(blob_id).await? {
                Some(s) => s,
                None => {
                    needs_fallback = true;
                    break;
                }
            };
            if sidecar.encryption.is_some() || sidecar.compression.is_some() {
                needs_fallback = true;
                break;
            }
            let md5_bytes = match hex::decode(&sidecar.etag) {
                Ok(b) if b.len() == 16 => b,
                _ => {
                    needs_fallback = true;
                    break;
                }
            };
            md5_concat.extend_from_slice(&md5_bytes);
            total_size += sidecar.size;
            composite_parts.push(CompositePart {
                blob_id: blob_id.clone(),
                plaintext_size: sidecar.size,
                plaintext_etag: sidecar.etag,
                encryption: None,
            });
        }

        if !needs_fallback {
            let etag = hex::encode(Md5::digest(&md5_concat));
            return Ok(BlobPutResult {
                size: total_size,
                etag,
                encryption: None,
                compression: None,
                composite_parts: Some(composite_parts),
            });
        }

        // Fallback: byte-by-byte copy with MD5 (preserved behaviour).
        let output_path = self.blob_path(output_blob_id);
        let tmp_path = self.tmp_path(output_blob_id);

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ArcaError::Internal(format!("create blob dir: {e}")))?;
        }

        let mut out_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("create tmp file: {e}")))?;

        let mut hasher = Md5::new();
        let mut total_size: u64 = 0;
        let mut buf = vec![0u8; 65536];

        for part_id in part_blob_ids {
            let part_path = self.blob_path(part_id);
            let mut part_file = fs::File::open(&part_path)
                .await
                .map_err(|e| ArcaError::Internal(format!("open part blob: {e}")))?;

            loop {
                let n = part_file.read(&mut buf)
                    .await
                    .map_err(|e| ArcaError::Internal(format!("read part blob: {e}")))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                total_size += n as u64;
                out_file.write_all(&buf[..n])
                    .await
                    .map_err(|e| ArcaError::Internal(format!("write concat blob: {e}")))?;
            }
        }

        out_file.flush()
            .await
            .map_err(|e| ArcaError::Internal(format!("flush concat blob: {e}")))?;
        drop(out_file);

        fs::rename(&tmp_path, &output_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("rename concat blob: {e}")))?;

        let etag = hex::encode(hasher.finalize());

        Ok(BlobPutResult { size: total_size, etag, encryption: None, compression: None, composite_parts: None })
    }

    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), ArcaError> {
        let sidecar_path = self.sidecar_path(blob_id);
        // Composite blobs never go through `put` (no on-disk file at this
        // blob_id), so the prefix directory may not exist yet. Make it.
        if let Some(parent) = sidecar_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| ArcaError::Internal(format!("create sidecar dir: {e}")))?;
        }
        let json = serde_json::to_string(meta)
            .map_err(|e| ArcaError::Internal(format!("serialize sidecar: {e}")))?;
        fs::write(&sidecar_path, json.as_bytes())
            .await
            .map_err(|e| ArcaError::Internal(format!("write sidecar: {e}")))?;
        Ok(())
    }
}

/// Raw verbatim access for cluster replication: delegates to the inherent
/// `read_raw`/`write_raw`/`exists`/`read_sidecar` methods and the `BlobStore`
/// sidecar writer. Exposing them behind `RawBlobOps` lets the arca-proto
/// cluster endpoints hold an `Arc<dyn RawBlobOps>` without depending on
/// arca-storage.
// Trait referenced by full path so its `write_sidecar` does not collide with
// `BlobStore::write_sidecar` for callers (e.g. unit tests) holding a concrete
// `FsBlobStore`. Production callers use `Arc<dyn RawBlobOps>`, never ambiguous.
#[async_trait::async_trait]
impl arca_core::store::RawBlobOps for FsBlobStore {
    async fn read_raw(&self, blob_id: &BlobId) -> Result<BlobGetResult, ArcaError> {
        FsBlobStore::read_raw(self, blob_id).await
    }

    async fn write_raw(&self, blob_id: &BlobId, stream: ByteStream) -> Result<u64, ArcaError> {
        FsBlobStore::write_raw(self, blob_id, stream).await
    }

    async fn exists(&self, blob_id: &BlobId) -> Result<bool, ArcaError> {
        FsBlobStore::exists(self, blob_id).await
    }

    async fn write_sidecar(&self, blob_id: &BlobId, meta: &SidecarMeta) -> Result<(), ArcaError> {
        BlobStore::write_sidecar(self, blob_id, meta).await
    }

    async fn read_sidecar(&self, blob_id: &BlobId) -> Result<Option<SidecarMeta>, ArcaError> {
        FsBlobStore::read_sidecar(self, blob_id).await
    }

    async fn list_blob_ids(&self) -> Result<Vec<(BlobId, std::time::SystemTime)>, ArcaError> {
        FsBlobStore::list_blob_files(self).await
    }

    async fn list_sidecar_ids(&self) -> Result<Vec<BlobId>, ArcaError> {
        FsBlobStore::list_sidecar_files(self).await
    }

    async fn delete_blob_file(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        FsBlobStore::delete_blob_file_raw(self, blob_id).await
    }
}

/// Recursively collects every regular file under `dir` as `(file_name, mtime)`.
/// Errors reading a subdirectory abort the walk (the GC caller treats a failed
/// enumeration as "skip this cycle", never as "nothing to keep").
fn walk_files(
    dir: PathBuf,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<Vec<(String, std::time::SystemTime)>, ArcaError>> + Send>,
> {
    Box::pin(async move {
        let mut out = Vec::new();
        let mut rd = match fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(ArcaError::Internal(format!("read_dir {dir:?}: {e}"))),
        };
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| ArcaError::Internal(format!("read_dir entry: {e}")))?
        {
            let ft = entry
                .file_type()
                .await
                .map_err(|e| ArcaError::Internal(format!("file_type: {e}")))?;
            if ft.is_dir() {
                out.extend(walk_files(entry.path()).await?);
            } else if ft.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                let mtime = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(std::time::SystemTime::now);
                out.push((name, mtime));
            }
        }
        Ok(out)
    })
}

/// Maps a `ReaderStream<R>` (which yields `Result<Bytes, io::Error>`) to a `ByteStream`.
fn map_reader_stream<R>(stream: ReaderStream<R>) -> impl Stream<Item = Result<bytes::Bytes, io::Error>>
where
    R: tokio::io::AsyncRead,
{
    stream
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio_stream::iter as stream_iter;

    /// Creates a ByteStream from a byte slice.
    fn bytes_to_stream(data: &[u8]) -> ByteStream {
        let chunks = vec![Ok(Bytes::copy_from_slice(data))];
        Box::pin(stream_iter(chunks))
    }

    /// Creates a FsBlobStore in a temp directory with the given prefix depth.
    async fn test_store(prefix_depth: u8) -> (FsBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FsBlobStore::new(dir.path().join("blobs"), prefix_depth)
            .await
            .unwrap();
        (store, dir)
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        let data = b"hello world";

        let put_result = store.put(&blob_id, bytes_to_stream(data)).await.unwrap();
        assert_eq!(put_result.size, data.len() as u64);

        let get_result = store.get(&blob_id, None).await.unwrap();
        assert_eq!(get_result.content_length, data.len() as u64);

        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, data);
    }

    #[tokio::test]
    async fn correct_etag() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        let data = b"hello world";

        let put_result = store.put(&blob_id, bytes_to_stream(data)).await.unwrap();

        // MD5 of "hello world"
        let expected = "5eb63bbbe01eeed093cb22bb8f5acdc3";
        assert_eq!(put_result.etag, expected);
    }

    #[tokio::test]
    async fn prefix_directory_structure() {
        let (store, dir) = test_store(2).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"test")).await.unwrap();

        let blob_path = store.blob_path(&blob_id);
        assert!(blob_path.exists());

        // Should have 2 levels of prefix dirs under blobs/
        let relative = blob_path.strip_prefix(dir.path().join("blobs")).unwrap();
        // e.g., "55/0e/550e8400-e29b-41d4-a716-446655440000" — 3 components
        assert_eq!(relative.components().count(), 3);
    }

    #[tokio::test]
    async fn prefix_depth_1() {
        let (store, dir) = test_store(1).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"test")).await.unwrap();

        let blob_path = store.blob_path(&blob_id);
        let relative = blob_path.strip_prefix(dir.path().join("blobs")).unwrap();
        assert_eq!(relative.components().count(), 2); // 1 prefix + filename
    }

    #[tokio::test]
    async fn prefix_depth_3() {
        let (store, dir) = test_store(3).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"test")).await.unwrap();

        let blob_path = store.blob_path(&blob_id);
        let relative = blob_path.strip_prefix(dir.path().join("blobs")).unwrap();
        assert_eq!(relative.components().count(), 4); // 3 prefixes + filename
    }

    #[tokio::test]
    async fn write_and_read_sidecar() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"data")).await.unwrap();

        let meta = SidecarMeta {
            bucket: "my-bucket".to_string(),
            key: "my-key".to_string(),
            size: 4,
            etag: "abc123".to_string(),
            content_type: Some("text/plain".to_string()),
            last_modified: "2024-01-01T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &meta).await.unwrap();

        let sidecar_path = store.sidecar_path(&blob_id);
        assert!(sidecar_path.exists());

        let content = std::fs::read_to_string(&sidecar_path).unwrap();
        let parsed: SidecarMeta = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.bucket, "my-bucket");
        assert_eq!(parsed.key, "my-key");
    }

    #[tokio::test]
    async fn delete_blob_and_sidecar() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"data")).await.unwrap();

        let meta = SidecarMeta {
            bucket: "b".to_string(),
            key: "k".to_string(),
            size: 4,
            etag: "e".to_string(),
            content_type: None,
            last_modified: "t".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &meta).await.unwrap();

        let blob_path = store.blob_path(&blob_id);
        let sidecar_path = store.sidecar_path(&blob_id);
        assert!(blob_path.exists());
        assert!(sidecar_path.exists());

        store.delete(&blob_id).await.unwrap();
        assert!(!blob_path.exists());
        assert!(!sidecar_path.exists());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_ok() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        // Should not error.
        store.delete(&blob_id).await.unwrap();
    }

    #[tokio::test]
    async fn range_read() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        let data = b"0123456789";
        store.put(&blob_id, bytes_to_stream(data)).await.unwrap();

        // Read bytes 3-6 (inclusive).
        let range = ByteRange {
            start: 3,
            end: Some(6),
        };
        let result = store.get(&blob_id, Some(range)).await.unwrap();
        assert_eq!(result.content_length, 4);

        let body = collect_stream(result.stream).await;
        assert_eq!(body, b"3456");
    }

    #[tokio::test]
    async fn range_read_open_end() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        let data = b"0123456789";
        store.put(&blob_id, bytes_to_stream(data)).await.unwrap();

        // Read from byte 7 to end.
        let range = ByteRange {
            start: 7,
            end: None,
        };
        let result = store.get(&blob_id, Some(range)).await.unwrap();
        assert_eq!(result.content_length, 3);

        let body = collect_stream(result.stream).await;
        assert_eq!(body, b"789");
    }

    #[tokio::test]
    async fn get_nonexistent_returns_error() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        let result = store.get(&blob_id, None).await;
        assert!(result.is_err());
    }

    // ----- Raw access (cluster replication) -----

    #[tokio::test]
    async fn raw_write_read_roundtrip() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        // Arbitrary bytes incl. non-UTF8 — verbatim, no interpretation.
        let data: &[u8] = b"\x00\x01\x02 raw ciphertext-like \xff\xfe bytes";
        let n = store.write_raw(&blob_id, bytes_to_stream(data)).await.unwrap();
        assert_eq!(n, data.len() as u64);
        assert!(store.exists(&blob_id).await.unwrap());

        let r = store.read_raw(&blob_id).await.unwrap();
        assert_eq!(r.content_length, data.len() as u64);
        let body = collect_stream(r.stream).await;
        assert_eq!(body, data);
    }

    #[tokio::test]
    async fn raw_write_is_idempotent_overwrite() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        store.write_raw(&blob_id, bytes_to_stream(b"same")).await.unwrap();
        // Re-deliver the same blob_id (idempotent replication): overwrites cleanly.
        store.write_raw(&blob_id, bytes_to_stream(b"same")).await.unwrap();
        let r = store.read_raw(&blob_id).await.unwrap();
        assert_eq!(collect_stream(r.stream).await, b"same");
    }

    #[tokio::test]
    async fn exists_false_for_missing() {
        let (store, _dir) = test_store(2).await;
        assert!(!store.exists(&BlobId::new()).await.unwrap());
    }

    #[tokio::test]
    async fn read_raw_missing_errors() {
        let (store, _dir) = test_store(2).await;
        assert!(store.read_raw(&BlobId::new()).await.is_err());
    }

    #[tokio::test]
    async fn raw_blob_ops_trait_roundtrip() {
        // Imported function-locally (not at module scope) so RawBlobOps'
        // write_sidecar does not collide with BlobStore's in the other tests.
        use arca_core::store::RawBlobOps;
        // Exercise RawBlobOps through a trait object — the exact path the
        // arca-proto cluster endpoints take (they hold an Arc<dyn RawBlobOps>).
        let (store, _dir) = test_store(2).await;
        let raw: std::sync::Arc<dyn RawBlobOps> = std::sync::Arc::new(store);
        let blob_id = BlobId::new();
        let data: &[u8] = b"verbatim-bytes";

        let n = raw.write_raw(&blob_id, bytes_to_stream(data)).await.unwrap();
        assert_eq!(n, data.len() as u64);
        assert!(raw.exists(&blob_id).await.unwrap());

        let meta = SidecarMeta {
            bucket: "b".to_string(),
            key: "k".to_string(),
            size: data.len() as u64,
            etag: "etag".to_string(),
            content_type: None,
            last_modified: "2026-05-31T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        };
        raw.write_sidecar(&blob_id, &meta).await.unwrap();

        let read_back = raw
            .read_sidecar(&blob_id)
            .await
            .unwrap()
            .expect("sidecar present");
        assert_eq!(read_back.bucket, "b");
        assert_eq!(read_back.key, "k");

        let r = raw.read_raw(&blob_id).await.unwrap();
        assert_eq!(r.content_length, data.len() as u64);
        assert_eq!(collect_stream(r.stream).await, data);
    }

    /// Helper to collect a ByteStream into a Vec<u8>.
    async fn collect_stream(stream: ByteStream) -> Vec<u8> {
        let mut stream = std::pin::pin!(stream);
        let mut buf = Vec::new();
        while let Some(chunk) = stream.as_mut().next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }

    // ----- Composite (multipart concat) tests for plain blobs -----

    /// Helper: write `n_parts` of `part_size` random-ish bytes as plain
    /// blobs (full sidecar). Returns the part blob ids and the
    /// concatenated plaintext that the composite blob should reproduce.
    async fn make_plain_parts(
        store: &FsBlobStore,
        n_parts: usize,
        part_size: usize,
    ) -> (Vec<BlobId>, Vec<u8>) {
        let mut ids = Vec::with_capacity(n_parts);
        let mut full = Vec::with_capacity(n_parts * part_size);
        for i in 0..n_parts {
            let blob_id = BlobId::new();
            let part_data: Vec<u8> = (0..part_size)
                .map(|b| ((i * 31 + b) % 256) as u8)
                .collect();
            let put_result = store.put(&blob_id, bytes_to_stream(&part_data)).await.unwrap();
            let sidecar = SidecarMeta {
                bucket: "test".to_string(),
                key: format!("part-{i}"),
                size: put_result.size,
                etag: put_result.etag.clone(),
                content_type: None,
                last_modified: "2026-01-01T00:00:00Z".to_string(),
                metadata: std::collections::HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            };
            store.write_sidecar(&blob_id, &sidecar).await.unwrap();
            ids.push(blob_id);
            full.extend_from_slice(&part_data);
        }
        (ids, full)
    }

    #[tokio::test]
    async fn concat_plain_returns_composite() {
        let (store, _dir) = test_store(2).await;
        let (part_ids, _full) = make_plain_parts(&store, 3, 64).await;
        let output_id = BlobId::new();

        let result = store.concat(&part_ids, &output_id).await.unwrap();

        // Composite path engaged: parts populated, no on-disk file written.
        let parts = result.composite_parts.expect("composite path should engage for plain parts");
        assert_eq!(parts.len(), 3);
        assert_eq!(result.size, 3 * 64);
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(p.blob_id, part_ids[i]);
            assert_eq!(p.plaintext_size, 64);
            assert!(p.encryption.is_none(), "plain composite must not carry encryption info");
        }
        assert!(!store.blob_path(&output_id).exists());
    }

    #[tokio::test]
    async fn concat_plain_composite_get_full_roundtrip() {
        let (store, _dir) = test_store(2).await;
        let (part_ids, full) = make_plain_parts(&store, 4, 256).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();

        // Caller writes the composite sidecar.
        let sidecar = SidecarMeta {
            bucket: "test".to_string(),
            key: "c.bin".to_string(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        let g = store.get(&output_id, None).await.unwrap();
        assert_eq!(g.content_length, full.len() as u64);
        let body = collect_stream(g.stream).await;
        assert_eq!(body, full);
    }

    #[tokio::test]
    async fn concat_plain_composite_range_within_part() {
        let (store, _dir) = test_store(2).await;
        let (part_ids, full) = make_plain_parts(&store, 3, 100).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".to_string(),
            key: "c.bin".to_string(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        // Range fully inside part 1 (covers byte 100..200).
        let g = store
            .get(&output_id, Some(ByteRange { start: 110, end: Some(130) }))
            .await
            .unwrap();
        assert_eq!(g.content_length, 21);
        let body = collect_stream(g.stream).await;
        assert_eq!(body, &full[110..=130]);
    }

    #[tokio::test]
    async fn concat_plain_composite_range_cross_two_parts() {
        let (store, _dir) = test_store(2).await;
        let (part_ids, full) = make_plain_parts(&store, 3, 100).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".to_string(),
            key: "c.bin".to_string(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        let g = store
            .get(&output_id, Some(ByteRange { start: 80, end: Some(150) }))
            .await
            .unwrap();
        assert_eq!(g.content_length, 71);
        let body = collect_stream(g.stream).await;
        assert_eq!(body, &full[80..=150]);
    }

    #[tokio::test]
    async fn concat_plain_composite_delete_cascades() {
        let (store, _dir) = test_store(2).await;
        let (part_ids, _full) = make_plain_parts(&store, 2, 64).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".to_string(),
            key: "c.bin".to_string(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        // Sanity: parts on disk before delete.
        for id in &part_ids {
            assert!(store.blob_path(id).exists());
        }

        store.delete(&output_id).await.unwrap();

        for id in &part_ids {
            assert!(!store.blob_path(id).exists(), "part {} should be gone", id.0);
        }
        assert!(store.get(&output_id, None).await.is_err());
    }

    #[tokio::test]
    async fn concat_falls_back_when_part_is_compressed() {
        // With a compressed part, the plain composite path can't carry the
        // compression metadata; we expect the byte-by-byte copy fallback.
        let (store, _dir) = test_store(2).await;
        let part_id = BlobId::new();
        let part_data = b"plaintext bytes";
        let put_result = store.put(&part_id, bytes_to_stream(part_data)).await.unwrap();
        // Forge a sidecar with a compression marker (the data is actually plain,
        // but the marker is enough to disqualify the composite fast path).
        let sidecar = SidecarMeta {
            bucket: "test".to_string(),
            key: "p".to_string(),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: Some(arca_core::store::BlobCompressionInfo {
                algorithm: arca_core::store::CompressionAlgorithm::Zstd,
                chunk_size: 65536,
                original_size: put_result.size,
                compressed_size: put_result.size,
            }),
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&part_id, &sidecar).await.unwrap();

        let output_id = BlobId::new();
        let result = store.concat(&[part_id], &output_id).await.unwrap();
        // Fallback engaged: composite_parts is None, an actual blob file exists.
        assert!(result.composite_parts.is_none());
        assert!(store.blob_path(&output_id).exists());
    }
}
