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

use super::temp::{temp_file_kind, TempFileKind};
use crate::composite::{concat_lazily, plan_composite_read, PartRead};

// ----- Durability helpers -----
//
// A write is acknowledged only after (1) the file data is fsynced, (2) the
// rename that publishes it is done and (3) the containing directory is
// fsynced, so a power loss cannot lose or truncate an acknowledged blob or
// sidecar. All of these block, so callers run them on `spawn_blocking`
// (or on tokio's file offload, which does the same).
//
// Blob files skip step (3): a blob is durable once its sidecar has been
// written. The sidecar lives in the same directory as the blob and
// `write_sidecar` fsyncs that directory after its own rename, which persists
// the blob's directory entry too. Every caller writes the sidecar (and fails
// on a sidecar error) before committing the metadata row, so 3 fsyncs per
// blob write instead of 4. See the `BlobStore::put` docs.

/// fsyncs a directory, persisting the entries (creations, renames) made in it.
pub fn sync_dir(dir: &std::path::Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// Removes a temp file when dropped, unless disarmed after the rename that
/// publishes it. Covers every way out of a write: an error return, and the
/// request future being dropped when the client disconnects, which no
/// `?`-path cleanup ever sees (TD-051). Arm it only once the file exists, so
/// it cannot run before a writer creates the file.
struct TempFileGuard(Option<PathBuf>);

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        TempFileGuard(Some(path))
    }

    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            // A blocking writer may still hold the file open: the unlink
            // still removes the name, and the bytes go away when it closes.
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(error = %e, path = %path.display(), "failed to remove temp blob"),
            }
        }
    }
}

/// Atomically publishes `tmp` as `dest` (rename) and fsyncs the parent
/// directory. The caller must already have fsynced `tmp`'s data.
pub fn rename_durable(tmp: &std::path::Path, dest: &std::path::Path) -> io::Result<()> {
    std::fs::rename(tmp, dest)?;
    match dest.parent() {
        Some(parent) => sync_dir(parent),
        None => Ok(()),
    }
}

/// Writes `bytes` to `dest` atomically and durably: temp file in the same
/// directory, fsync, rename, directory fsync. On failure the temp file is
/// removed and any existing `dest` is left untouched.
pub fn write_file_atomic(dest: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    write_file_atomic_with(dest, bytes, true)
}

/// [`write_file_atomic`], with the two fsyncs skipped when `fsync` is false
/// (`[storage] fsync = false`): still atomic, no longer durable.
fn write_file_atomic_with(dest: &std::path::Path, bytes: &[u8], fsync: bool) -> io::Result<()> {
    use std::io::Write as _;
    let mut name = dest.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
    let tmp = dest.with_file_name(name);
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        if !fsync {
            drop(file);
            return std::fs::rename(&tmp, dest);
        }
        file.sync_all()?;
        drop(file);
        rename_durable(&tmp, dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Creates `dir` (and missing ancestors) and fsyncs every directory that
/// gained an entry, so freshly created shard directories survive a power loss.
/// The common case (directory already exists) is a single async `stat`.
/// With `fsync` false it is a plain `create_dir_all`.
pub(crate) async fn create_dir_all_durable(dir: &std::path::Path, fsync: bool) -> io::Result<()> {
    if !fsync {
        return fs::create_dir_all(dir).await;
    }
    if fs::try_exists(dir).await? {
        return Ok(());
    }
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut missing = Vec::new();
        let mut existing = dir.as_path();
        while !existing.exists() {
            missing.push(existing);
            match existing.parent() {
                Some(p) => existing = p,
                None => break,
            }
        }
        std::fs::create_dir_all(&dir)?;
        sync_dir(existing)?;
        for d in missing.iter().rev() {
            sync_dir(d)?;
        }
        Ok(())
    })
    .await
    .map_err(|e| io::Error::other(e.to_string()))?
}


/// Filesystem-backed blob store.
///
/// File name `blob_path` yields for an id that is not a UUID. Never a blob.
const INVALID_BLOB_ID_NAME: &str = "_invalid-blob-id";

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
    /// Whether writes are fsynced (`[storage] fsync`, default true). Only the
    /// live server can turn it off; the offline tools keep the default.
    fsync: bool,
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
            fsync: true,
        })
    }

    /// Turns the write-path fsyncs on or off (`[storage] fsync`). Off, every
    /// writer keeps its temp + rename protocol but skips the fsyncs, so an
    /// acknowledged write can be lost or truncated by a power loss.
    pub fn with_fsync(mut self, fsync: bool) -> Self {
        self.fsync = fsync;
        self
    }

    /// Whether writes are fsynced.
    pub fn fsync(&self) -> bool {
        self.fsync
    }

    /// Computes the full path for a blob ID using the configured prefix depth.
    ///
    /// Total and safe for any input: an id that is not a UUID (TD-041: ids
    /// reach this layer from cluster peers and from sidecars) maps to a fixed
    /// name inside the base directory that no blob ever uses, so it can
    /// neither traverse out of the tree nor panic on a byte-index slice. The
    /// operations that take peer-supplied ids reject them first
    /// ([`Self::checked_blob_path`]); the rest then simply find no file.
    pub fn blob_path(&self, blob_id: &BlobId) -> PathBuf {
        if !blob_id.is_valid() {
            return self.base_dir.join(INVALID_BLOB_ID_NAME);
        }
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

    /// [`Self::blob_path`], refusing an id that is not a UUID.
    fn checked_blob_path(&self, blob_id: &BlobId) -> Result<PathBuf, ArcaError> {
        self.check_blob_id(blob_id)?;
        Ok(self.blob_path(blob_id))
    }

    fn check_blob_id(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        if blob_id.is_valid() {
            Ok(())
        } else {
            Err(ArcaError::Internal(format!(
                "invalid blob id {:?} (not a UUID)",
                blob_id.0
            )))
        }
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
        let blob_path = self.checked_blob_path(blob_id)?;
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
        let blob_path = self.checked_blob_path(blob_id)?;
        if let Some(parent) = blob_path.parent() {
            create_dir_all_durable(parent, self.fsync)
                .await
                .map_err(|e| ArcaError::Internal(format!("create blob dir: {e}")))?;
        }
        // A temp name of its own (not `tmp_path`): two deliveries of the same
        // blob (a replication push and an anti-entropy repair) may overlap,
        // and must neither truncate nor clean up each other's file.
        let mut tmp_name = blob_path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        let tmp_path = blob_path.with_file_name(tmp_name);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("create raw tmp: {e}")))?;
        let guard = TempFileGuard::new(tmp_path.clone());
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
        if self.fsync {
            file.sync_all()
                .await
                .map_err(|e| ArcaError::Internal(format!("fsync raw blob: {e}")))?;
        }
        drop(file);
        // No directory fsync: a blob is durable once its sidecar has been
        // written (the caller ships it next, same directory).
        fs::rename(&tmp_path, &blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("rename raw blob: {e}")))?;
        guard.disarm();
        Ok(size)
    }

    /// Whether the blob's physical file exists (anti-entropy probe).
    pub async fn exists(&self, blob_id: &BlobId) -> Result<bool, ArcaError> {
        match fs::metadata(self.checked_blob_path(blob_id)?).await {
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
            .filter(|f| !f.name.ends_with(".meta") && !f.name.ends_with(".tmp"))
            .map(|f| (BlobId(f.name), f.mtime))
            // Not a blob of this store (a stray file): `arca fsck` reports it.
            .filter(|(id, _)| id.is_valid())
            .collect())
    }

    /// See [`RawBlobOps::list_reclaimable_temp_files`].
    pub async fn list_reclaimable_temp_files(
        &self,
    ) -> Result<Vec<(PathBuf, std::time::SystemTime)>, ArcaError> {
        let files = walk_files(self.base_dir.clone()).await?;
        Ok(files
            .into_iter()
            .filter(|f| temp_file_kind(&f.name) == Some(TempFileKind::Reclaimable))
            .map(|f| (f.path, f.mtime))
            .collect())
    }

    /// See [`RawBlobOps::delete_temp_file`].
    pub async fn delete_temp_file(&self, path: &std::path::Path) -> Result<(), ArcaError> {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !path.starts_with(&self.base_dir) || temp_file_kind(name) != Some(TempFileKind::Reclaimable) {
            return Err(ArcaError::Internal(format!(
                "refusing to delete {}: not a reclaimable temp file of this store",
                path.display()
            )));
        }
        match fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(ArcaError::Internal(format!("delete temp file: {e}"))),
        }
    }

    /// Lists the blob ids of every sidecar (`.meta`) on disk.
    pub async fn list_sidecar_files(&self) -> Result<Vec<BlobId>, ArcaError> {
        let files = walk_files(self.base_dir.clone()).await?;
        Ok(files
            .into_iter()
            .filter_map(|f| f.name.strip_suffix(".meta").map(|s| BlobId(s.to_string())))
            .filter(BlobId::is_valid)
            .collect())
    }

    /// Deletes one physical blob file and its sidecar (no composite cascade).
    pub async fn delete_blob_file_raw(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        self.check_blob_id(blob_id)?;
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
        self.check_blob_id(blob_id)?;
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
    /// we read only the byte ranges that overlap the request, opening one
    /// part file at a time (see [`concat_lazily`]).
    async fn get_composite(
        &self,
        parts: &[CompositePart],
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        let (reads, content_length) = plan_composite_read(parts, range);
        let store = self.clone();
        let stream = concat_lazily(reads, move |read: PartRead| {
            let store = store.clone();
            async move {
                // The part is a normal blob (no encryption layer at this
                // level), so reading its file is enough.
                let part = store.get_part_data(&read.part.blob_id, read.range).await?;
                Ok(part.stream)
            }
        })
        .await?;

        Ok(BlobGetResult {
            stream,
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
            create_dir_all_durable(parent, self.fsync)
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
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("create tmp file: {e}")))?
            .into_std()
            .await;
        let guard = TempFileGuard::new(tmp_path.clone());
        let fsync = self.fsync;
        let worker = tokio::task::spawn_blocking(move || -> io::Result<(u64, [u8; 16])> {
            use std::io::Write as _;
            let mut file = file;
            let mut hasher = Md5::new();
            let mut size: u64 = 0;
            while let Some(chunk) = rx.blocking_recv() {
                hasher.update(&chunk);
                size += chunk.len() as u64;
                file.write_all(&chunk)?;
            }
            file.flush()?;
            // Data must be on disk before the rename publishes the blob.
            if fsync {
                file.sync_all()?;
            }
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

        let written = worker
            .await
            .map_err(|e| ArcaError::Internal(format!("blob writer join: {e}")))
            .and_then(|r| r.map_err(|e| ArcaError::Internal(format!("write blob: {e}"))));

        // A failed write (body error, size limit, disk error) leaves nothing
        // behind: the guard removes the partial temp file.
        let (size, digest) = match (written, stream_err) {
            (Ok(done), None) => done,
            (Ok(_), Some(e)) | (Err(e), _) => return Err(e),
        };

        // Atomic rename: tmp → final. No directory fsync: a blob is durable
        // once its sidecar has been written (same directory, see above).
        fs::rename(&tmp_path, &blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("rename blob: {e}")))?;
        guard.disarm();

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

    async fn delete_assembled(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        self.delete_blob_file_raw(blob_id).await
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
            create_dir_all_durable(parent, self.fsync)
                .await
                .map_err(|e| ArcaError::Internal(format!("create blob dir: {e}")))?;
        }

        let mut out_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("create tmp file: {e}")))?;
        let guard = TempFileGuard::new(tmp_path.clone());

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
        if self.fsync {
            out_file.sync_all()
                .await
                .map_err(|e| ArcaError::Internal(format!("fsync concat blob: {e}")))?;
        }
        drop(out_file);

        // No directory fsync: a blob is durable once its sidecar has been
        // written (CompleteMultipartUpload writes it next, same directory).
        fs::rename(&tmp_path, &output_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("rename concat blob: {e}")))?;
        guard.disarm();

        let etag = hex::encode(hasher.finalize());

        Ok(BlobPutResult { size: total_size, etag, encryption: None, compression: None, composite_parts: None })
    }

    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), ArcaError> {
        self.check_blob_id(blob_id)?;
        let sidecar_path = self.sidecar_path(blob_id);
        // `write_file_atomic` fsyncs the directory after the rename: that one
        // fsync also persists the entry of the blob published next to it by
        // `put` / `write_raw` / `concat`, which skip their own.
        //
        // Composite blobs never go through `put` (no on-disk file at this
        // blob_id), so the prefix directory may not exist yet. Make it.
        if let Some(parent) = sidecar_path.parent() {
            create_dir_all_durable(parent, self.fsync)
                .await
                .map_err(|e| ArcaError::Internal(format!("create sidecar dir: {e}")))?;
        }
        let json = serde_json::to_string(meta)
            .map_err(|e| ArcaError::Internal(format!("serialize sidecar: {e}")))?;
        let fsync = self.fsync;
        tokio::task::spawn_blocking(move || write_file_atomic_with(&sidecar_path, json.as_bytes(), fsync))
            .await
            .map_err(|e| ArcaError::Internal(format!("sidecar writer join: {e}")))?
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

    async fn list_reclaimable_temp_files(
        &self,
    ) -> Result<Vec<(PathBuf, std::time::SystemTime)>, ArcaError> {
        FsBlobStore::list_reclaimable_temp_files(self).await
    }

    async fn delete_temp_file(&self, path: &std::path::Path) -> Result<(), ArcaError> {
        FsBlobStore::delete_temp_file(self, path).await
    }

    async fn delete_blob_file(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        FsBlobStore::delete_blob_file_raw(self, blob_id).await
    }
}

/// Recursively collects every regular file under `dir` as `(file_name, mtime)`.
/// Errors reading a subdirectory abort the walk (the GC caller treats a failed
/// enumeration as "skip this cycle", never as "nothing to keep").
/// A file found by [`walk_files`].
struct WalkedFile {
    path: PathBuf,
    name: String,
    mtime: std::time::SystemTime,
}

fn walk_files(
    dir: PathBuf,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<WalkedFile>, ArcaError>> + Send>> {
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
                let mtime = match entry.metadata().await.ok().and_then(|m| m.modified().ok()) {
                    Some(t) => t,
                    None => {
                        // Falling back to `now` is the safe direction for the
                        // GC (a blob that always looks fresh is never
                        // reclaimed) — but silently so on a filesystem without
                        // mtimes the GC would never collect anything.
                        tracing::warn!(
                            file = %name,
                            "blob walk: file mtime unreadable; treating it as just \
                             written — if this filesystem cannot report mtimes, blob \
                             GC will never reclaim anything here"
                        );
                        std::time::SystemTime::now()
                    }
                };
                out.push(WalkedFile { path: entry.path(), name, mtime });
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
    async fn failed_stream_leaves_no_blob_or_temp_file() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        let chunks = vec![
            Ok(Bytes::from_static(b"partial body")),
            Err(io::Error::new(io::ErrorKind::Other, "EntityTooLarge")),
        ];
        let err = store
            .put(&blob_id, Box::pin(stream_iter(chunks)))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("EntityTooLarge"));
        assert!(!store.tmp_path(&blob_id).exists(), "temp file must be removed");
        assert!(!store.blob_path(&blob_id).exists(), "no blob must be stored");
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

    /// Helper: writes the composite sidecar for `part_ids` (through `concat`)
    /// and returns the composite blob id.
    async fn make_plain_composite(store: &FsBlobStore, part_ids: &[BlobId]) -> BlobId {
        let output_id = BlobId::new();
        let result = store.concat(part_ids, &output_id).await.unwrap();
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
        output_id
    }

    /// Collects a stream until its first error: the bytes read before it, and
    /// whether it ended with an error.
    async fn collect_until_error(stream: ByteStream) -> (Vec<u8>, bool) {
        let mut stream = std::pin::pin!(stream);
        let mut buf = Vec::new();
        while let Some(chunk) = stream.as_mut().next().await {
            match chunk {
                Ok(c) => buf.extend_from_slice(&c),
                Err(_) => return (buf, true),
            }
        }
        (buf, false)
    }

    #[tokio::test]
    async fn composite_get_opens_parts_lazily() {
        // A GET of a composite must not open every part up front: a 200 GB
        // object of 5995 parts needed 5995 file descriptors per request and
        // failed with EMFILE (os error 24). Probe: once the stream exists,
        // unlink the last part. A part file opened eagerly would still be
        // readable through its open descriptor; a lazily opened one is gone.
        let (store, _dir) = test_store(2).await;
        let (part_ids, full) = make_plain_parts(&store, 3, 100).await;
        let output_id = make_plain_composite(&store, &part_ids).await;

        let g = store.get(&output_id, None).await.unwrap();
        assert_eq!(g.content_length, 300);
        std::fs::remove_file(store.blob_path(&part_ids[2])).unwrap();

        let (body, failed) = collect_until_error(g.stream).await;
        assert!(failed, "the unlinked last part must not have been opened before the GET reached it");
        assert_eq!(body, &full[..200], "the parts before it are streamed in full");
    }

    #[tokio::test]
    async fn composite_range_get_opens_parts_lazily() {
        let (store, _dir) = test_store(2).await;
        let (part_ids, full) = make_plain_parts(&store, 4, 100).await;
        let output_id = make_plain_composite(&store, &part_ids).await;

        // Bytes 150..=349: the tail of part 1, all of part 2, the head of part 3.
        let g = store
            .get(&output_id, Some(ByteRange { start: 150, end: Some(349) }))
            .await
            .unwrap();
        assert_eq!(g.content_length, 200);
        std::fs::remove_file(store.blob_path(&part_ids[3])).unwrap();

        let (body, failed) = collect_until_error(g.stream).await;
        assert!(failed);
        assert_eq!(body, &full[150..300]);
    }

    #[tokio::test]
    async fn composite_get_with_missing_first_part_fails_before_streaming() {
        // The first part is still opened before `get` returns, so the common
        // failure (a missing part) remains an error response, not a 200 whose
        // body breaks off.
        let (store, _dir) = test_store(2).await;
        let (part_ids, _full) = make_plain_parts(&store, 3, 100).await;
        let output_id = make_plain_composite(&store, &part_ids).await;
        std::fs::remove_file(store.blob_path(&part_ids[0])).unwrap();

        assert!(store.get(&output_id, None).await.is_err());
        // Same for the first part a range touches.
        std::fs::remove_file(store.blob_path(&part_ids[1])).unwrap();
        assert!(store
            .get(&output_id, Some(ByteRange { start: 150, end: Some(160) }))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn composite_get_with_thousands_of_parts() {
        // More parts than the common 1024 soft limit on open files. The part
        // files are written directly (no sidecar, no fsync) to keep this fast;
        // `get` reads only the composite sidecar.
        const PARTS: usize = 5000;
        let (store, _dir) = test_store(2).await;
        let mut composite = Vec::with_capacity(PARTS);
        let mut full = Vec::with_capacity(PARTS * 4);
        for i in 0..PARTS {
            let blob_id = BlobId::new();
            let data = (i as u32).to_be_bytes();
            let path = store.blob_path(&blob_id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, data).unwrap();
            full.extend_from_slice(&data);
            composite.push(CompositePart {
                blob_id,
                plaintext_size: 4,
                plaintext_etag: String::new(),
                encryption: None,
            });
        }
        let output_id = BlobId::new();
        let sidecar = SidecarMeta {
            bucket: "test".to_string(),
            key: "many.bin".to_string(),
            size: full.len() as u64,
            etag: String::new(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: Some(composite),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        let g = store.get(&output_id, None).await.unwrap();
        assert_eq!(g.content_length, full.len() as u64);
        assert_eq!(collect_stream(g.stream).await, full);

        let g = store
            .get(&output_id, Some(ByteRange { start: 2, end: Some(19_997) }))
            .await
            .unwrap();
        assert_eq!(g.content_length, 19_996);
        assert_eq!(collect_stream(g.stream).await, &full[2..=19_997]);
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

    #[tokio::test]
    async fn concat_falls_back_when_a_part_has_no_sidecar() {
        // A part without a sidecar (what UploadPartCopy used to leave for a
        // plain part) cannot join a composite: its etag/size are unknown here,
        // so concat copies the bytes. Every part writer must write one.
        let (store, _dir) = test_store(2).await;
        let (mut part_ids, mut full) = make_plain_parts(&store, 1, 64).await;
        let bare = BlobId::new();
        store.put(&bare, bytes_to_stream(b"no sidecar")).await.unwrap();
        part_ids.push(bare);
        full.extend_from_slice(b"no sidecar");

        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();

        assert!(result.composite_parts.is_none());
        assert_eq!(std::fs::read(store.blob_path(&output_id)).unwrap(), full);
    }

    #[tokio::test]
    async fn sidecar_lives_in_the_blob_directory_at_every_depth() {
        // The single directory fsync done by write_sidecar covers the blob's
        // own directory entry only because both live in the same directory.
        for depth in 1..=4 {
            let (store, _dir) = test_store(depth).await;
            let blob_id = BlobId::new();
            assert_eq!(
                store.blob_path(&blob_id).parent(),
                store.sidecar_path(&blob_id).parent(),
                "depth {depth}"
            );
        }
    }

    fn test_sidecar(etag: &str) -> SidecarMeta {
        SidecarMeta {
            bucket: "b".to_string(),
            key: "k".to_string(),
            size: 1,
            etag: etag.to_string(),
            content_type: None,
            last_modified: "t".to_string(),
            metadata: std::collections::HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    /// Names of every file in the directory holding `blob_id`'s files.
    fn dir_entries(store: &FsBlobStore, blob_id: &BlobId) -> Vec<String> {
        let dir = store.blob_path(blob_id).parent().unwrap().to_path_buf();
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    // fsync itself is not observable from a unit test (it only matters on
    // power loss); these tests cover the atomic temp+rename protocol around it.

    #[tokio::test]
    async fn put_leaves_no_temp_file() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"data")).await.unwrap();
        assert_eq!(dir_entries(&store, &blob_id), vec![blob_id.0.clone()]);
    }

    #[tokio::test]
    async fn write_sidecar_is_atomic_and_leaves_no_temp_file() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"data")).await.unwrap();

        store.write_sidecar(&blob_id, &test_sidecar("one")).await.unwrap();
        store.write_sidecar(&blob_id, &test_sidecar("two")).await.unwrap();

        let read = store.read_sidecar(&blob_id).await.unwrap().unwrap();
        assert_eq!(read.etag, "two");
        let mut entries = dir_entries(&store, &blob_id);
        entries.sort();
        let mut expected = vec![blob_id.0.clone(), format!("{}.meta", blob_id.0)];
        expected.sort();
        assert_eq!(entries, expected, "no .tmp file may remain");
    }

    #[tokio::test]
    async fn failed_sidecar_write_leaves_target_and_no_temp_file() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"data")).await.unwrap();
        // Make the final rename fail: the sidecar path is a non-empty directory.
        let sidecar_path = store.sidecar_path(&blob_id);
        std::fs::create_dir(&sidecar_path).unwrap();
        std::fs::write(sidecar_path.join("keep"), b"x").unwrap();

        let err = store.write_sidecar(&blob_id, &test_sidecar("x")).await;
        assert!(err.is_err());
        assert!(sidecar_path.join("keep").exists(), "existing target untouched");
        assert!(
            !dir_entries(&store, &blob_id).iter().any(|n| n.ends_with(".tmp")),
            "temp file must be cleaned up on failure"
        );
    }

    #[tokio::test]
    async fn write_sidecar_creates_missing_shard_directories() {
        let (store, _dir) = test_store(3).await;
        let blob_id = BlobId::new();
        store.write_sidecar(&blob_id, &test_sidecar("e")).await.unwrap();
        assert!(store.read_sidecar(&blob_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn write_raw_leaves_no_temp_file() {
        let (store, _dir) = test_store(2).await;
        let raw_id = BlobId::new();
        store.write_raw(&raw_id, bytes_to_stream(b"raw")).await.unwrap();
        assert_eq!(dir_entries(&store, &raw_id), vec![raw_id.0.clone()]);
    }

    /// One chunk, then a body that never ends: the client stalled.
    fn stalled_stream() -> ByteStream {
        let first = futures_util::stream::iter(vec![Ok(Bytes::from_static(b"partial"))]);
        Box::pin(futures_util::StreamExt::chain(first, futures_util::stream::pending()))
    }

    /// Drops `fut` once it has been pending for a while, as hyper does when
    /// the client disconnects, then lets any blocking writer finish.
    async fn cancel<F: std::future::Future>(fut: F) {
        let result = tokio::time::timeout(std::time::Duration::from_millis(100), fut).await;
        assert!(result.is_err(), "the write must still be pending when cancelled");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn cancelled_put_leaves_no_temp_file() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        cancel(store.put(&blob_id, stalled_stream())).await;
        assert!(dir_entries(&store, &blob_id).is_empty(), "{:?}", dir_entries(&store, &blob_id));
    }

    #[tokio::test]
    async fn cancelled_write_raw_leaves_no_temp_file() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        cancel(store.write_raw(&blob_id, stalled_stream())).await;
        assert!(dir_entries(&store, &blob_id).is_empty(), "{:?}", dir_entries(&store, &blob_id));
    }

    #[tokio::test]
    async fn overlapping_write_raw_of_one_blob_do_not_share_a_temp_file() {
        let (store, _dir) = test_store(2).await;
        let blob_id = BlobId::new();
        // Delivery A stalls and is cancelled at 50 ms; delivery B of the same
        // blob starts at 20 ms and finishes at about 120 ms. With one shared
        // temp path, A's cleanup removed B's file and B's rename failed.
        let a = tokio::time::timeout(std::time::Duration::from_millis(50), store.write_raw(&blob_id, stalled_stream()));
        let b = async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let slow: ByteStream = Box::pin(futures_util::stream::unfold(0u8, |step| async move {
                match step {
                    0 => Some((Ok(Bytes::from_static(b"full ")), 1)),
                    1 => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        Some((Ok(Bytes::from_static(b"blob")), 2))
                    }
                    _ => None,
                }
            }));
            store.write_raw(&blob_id, slow).await
        };
        let (cancelled, done) = tokio::join!(a, b);
        assert!(cancelled.is_err(), "A must still be pending when cancelled");
        assert_eq!(done.unwrap(), 9);
        assert_eq!(std::fs::read(store.blob_path(&blob_id)).unwrap(), b"full blob");
        assert_eq!(dir_entries(&store, &blob_id), vec![blob_id.0.clone()]);
    }

    #[tokio::test]
    async fn failed_concat_fallback_leaves_no_temp_file() {
        let (store, _dir) = test_store(2).await;
        // A part without a sidecar forces the byte copy; the second part
        // does not exist, so the copy fails half-way.
        let bare = BlobId::new();
        store.put(&bare, bytes_to_stream(b"no sidecar")).await.unwrap();
        let output_id = BlobId::new();
        assert!(store.concat(&[bare, BlobId::new()], &output_id).await.is_err());
        let tmp = store.tmp_path(&output_id);
        assert!(!tmp.exists(), "temp file left: {}", tmp.display());
    }

    #[tokio::test]
    async fn lists_only_reclaimable_temp_files() {
        let (store, _dir) = test_store(2).await;
        let id = BlobId::new();
        store.put(&id, bytes_to_stream(b"data")).await.unwrap();
        let blob = store.blob_path(&id);
        let named = |suffix: &str| blob.with_file_name(format!("{}{suffix}", id.0));
        for suffix in [".tmp", ".meta.0123456789abcdef0123456789abcdef.tmp", ".rewrite.tmp", ".meta.pending.tmp"] {
            std::fs::write(named(suffix), b"partial").unwrap();
        }

        let mut got: Vec<PathBuf> = store.list_reclaimable_temp_files().await.unwrap().into_iter().map(|(p, _)| p).collect();
        got.sort();
        let mut want = vec![named(".tmp"), named(".meta.0123456789abcdef0123456789abcdef.tmp")];
        want.sort();
        assert_eq!(got, want);
    }

    #[tokio::test]
    async fn delete_temp_file_refuses_anything_but_a_reclaimable_temp_file() {
        let (store, _dir) = test_store(2).await;
        let id = BlobId::new();
        store.put(&id, bytes_to_stream(b"data")).await.unwrap();
        let blob = store.blob_path(&id);
        let rewrite = blob.with_file_name(format!("{}.rewrite.tmp", id.0));
        std::fs::write(&rewrite, b"recovery state").unwrap();
        let outside = _dir.path().join("elsewhere.tmp");
        std::fs::write(&outside, b"x").unwrap();

        for path in [&blob, &rewrite, &outside] {
            assert!(store.delete_temp_file(path).await.is_err(), "{}", path.display());
            assert!(path.exists(), "{}", path.display());
        }
        let tmp = blob.with_file_name(format!("{}.tmp", id.0));
        std::fs::write(&tmp, b"partial").unwrap();
        store.delete_temp_file(&tmp).await.unwrap();
        assert!(!tmp.exists());
        store.delete_temp_file(&tmp).await.unwrap(); // already gone is fine
    }

    #[tokio::test]
    async fn new_store_fsyncs_by_default() {
        // Offline tools build the store with `new`: they are always durable.
        let (store, _dir) = test_store(2).await;
        assert!(store.fsync());
        assert!(!store.with_fsync(false).fsync());
    }

    #[tokio::test]
    async fn fsync_off_keeps_every_writer_working_and_atomic() {
        // `[storage] fsync = false` only drops the fsyncs: the temp + rename
        // protocol (no partial sidecar, no stray temp file) stays.
        let (store, _dir) = test_store(3).await;
        let store = store.with_fsync(false);

        let blob_id = BlobId::new();
        store.put(&blob_id, bytes_to_stream(b"data")).await.unwrap();
        store.write_sidecar(&blob_id, &test_sidecar("one")).await.unwrap();
        store.write_sidecar(&blob_id, &test_sidecar("two")).await.unwrap();
        assert_eq!(store.read_sidecar(&blob_id).await.unwrap().unwrap().etag, "two");
        let mut entries = dir_entries(&store, &blob_id);
        entries.sort();
        let mut expected = vec![blob_id.0.clone(), format!("{}.meta", blob_id.0)];
        expected.sort();
        assert_eq!(entries, expected, "no .tmp file may remain");

        let raw_id = BlobId::new();
        store.write_raw(&raw_id, bytes_to_stream(b"raw")).await.unwrap();
        assert_eq!(dir_entries(&store, &raw_id), vec![raw_id.0.clone()]);

        // concat fallback path (a part without a sidecar forces a byte copy).
        let bare = BlobId::new();
        store.put(&bare, bytes_to_stream(b"no sidecar")).await.unwrap();
        let output_id = BlobId::new();
        let result = store.concat(&[bare], &output_id).await.unwrap();
        assert!(result.composite_parts.is_none());
        assert_eq!(std::fs::read(store.blob_path(&output_id)).unwrap(), b"no sidecar");
    }

    /// Ids a cluster peer could send: none may reach the file system.
    fn hostile_ids() -> Vec<BlobId> {
        ["", "..", "../../etc/passwd", "a/b", "ab", "\u{e9}\u{e9}\u{e9}", "blob-1"]
            .into_iter()
            .map(|s| BlobId(s.to_string()))
            .collect()
    }

    #[tokio::test]
    async fn blob_path_never_leaves_the_base_dir_nor_panics() {
        let (store, _dir) = test_store(2).await;
        for id in hostile_ids() {
            let path = store.blob_path(&id);
            assert!(path.starts_with(&store.base_dir), "{id:?} escaped: {path:?}");
            assert!(
                !path.components().any(|c| matches!(c, std::path::Component::ParentDir)),
                "{id:?} kept a parent component: {path:?}"
            );
            // sidecar_path must not panic either.
            let _ = store.sidecar_path(&id);
        }
    }

    #[tokio::test]
    async fn raw_ops_reject_non_uuid_blob_ids() {
        use arca_core::store::RawBlobOps;
        let (store, _dir) = test_store(2).await;
        let sidecar = SidecarMeta {
            bucket: "b".into(),
            key: "k".into(),
            size: 0,
            etag: String::new(),
            content_type: None,
            last_modified: String::new(),
            metadata: Default::default(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        };
        for id in hostile_ids() {
            assert!(RawBlobOps::read_raw(&store, &id).await.is_err(), "read_raw {id:?}");
            assert!(
                RawBlobOps::write_raw(&store, &id, bytes_to_stream(b"x")).await.is_err(),
                "write_raw {id:?}"
            );
            assert!(RawBlobOps::exists(&store, &id).await.is_err(), "exists {id:?}");
            assert!(RawBlobOps::read_sidecar(&store, &id).await.is_err(), "read_sidecar {id:?}");
            assert!(
                RawBlobOps::write_sidecar(&store, &id, &sidecar).await.is_err(),
                "write_sidecar {id:?}"
            );
            assert!(RawBlobOps::delete_blob_file(&store, &id).await.is_err(), "delete {id:?}");
        }
        // Nothing was created outside or inside the blobs tree.
        assert!(store.list_blob_files().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn listings_skip_files_that_are_not_arca_blobs() {
        let (store, dir) = test_store(2).await;
        let blobs = dir.path().join("blobs");
        let id = BlobId::new();
        store.write_raw(&id, bytes_to_stream(b"x")).await.unwrap();
        // A stray file, a stray ".meta" (it would be read as a sidecar) and one
        // nested in a shard directory: none is a blob of this store.
        std::fs::write(blobs.join("notes.txt"), b"n").unwrap();
        std::fs::write(blobs.join("backup.meta"), b"{}").unwrap();
        let shard = store.blob_path(&id).parent().unwrap().to_path_buf();
        std::fs::write(shard.join(".DS_Store"), b"n").unwrap();

        let blobs_listed: Vec<_> = store.list_blob_files().await.unwrap().into_iter().map(|(b, _)| b).collect();
        assert_eq!(blobs_listed, vec![id]);
        assert!(store.list_sidecar_files().await.unwrap().is_empty());
    }
}
