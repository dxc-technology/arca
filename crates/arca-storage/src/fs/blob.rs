//! `BlobStore` implementation backed by the local filesystem.

use std::io;
use std::path::PathBuf;

use futures_core::Stream;
use md5::{Digest, Md5};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_stream::StreamExt;
use tokio_util::io::ReaderStream;

use arca_core::error::ArcaError;
use arca_core::store::{BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream, SidecarMeta};
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

        // Open temp file (create new, exclusive).
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("create tmp file: {e}")))?;

        let mut hasher = Md5::new();
        let mut size: u64 = 0;

        // Stream chunks: update MD5 hasher and write to file concurrently.
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.as_mut().next().await {
            let chunk = chunk.map_err(|e| ArcaError::Internal(format!("read stream: {e}")))?;
            hasher.update(&chunk);
            size += chunk.len() as u64;
            file.write_all(&chunk)
                .await
                .map_err(|e| ArcaError::Internal(format!("write blob: {e}")))?;
        }

        file.flush()
            .await
            .map_err(|e| ArcaError::Internal(format!("flush blob: {e}")))?;
        drop(file);

        // Atomic rename: tmp → final.
        fs::rename(&tmp_path, &blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("rename blob: {e}")))?;

        let etag = hex::encode(hasher.finalize());

        Ok(BlobPutResult { size, etag, encryption: None })
    }

    async fn get(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
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

    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), ArcaError> {
        let sidecar_path = self.sidecar_path(blob_id);
        let json = serde_json::to_string(meta)
            .map_err(|e| ArcaError::Internal(format!("serialize sidecar: {e}")))?;
        fs::write(&sidecar_path, json.as_bytes())
            .await
            .map_err(|e| ArcaError::Internal(format!("write sidecar: {e}")))?;
        Ok(())
    }
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
            version_id: None,
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
            version_id: None,
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

    /// Helper to collect a ByteStream into a Vec<u8>.
    async fn collect_stream(stream: ByteStream) -> Vec<u8> {
        let mut stream = std::pin::pin!(stream);
        let mut buf = Vec::new();
        while let Some(chunk) = stream.as_mut().next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }
}
