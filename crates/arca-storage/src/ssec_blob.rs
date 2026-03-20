//! SSE-C (Server-Side Encryption with Customer-provided keys) blob operations.
//!
//! Unlike `EncryptingBlobStore`, the customer key is per-call (not per-instance),
//! so `SsecBlobStore` does NOT implement the `BlobStore` trait.

use bytes::Bytes;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use arca_core::error::ArcaError;
use arca_core::store::{BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream, SidecarMeta, SsecBlobOps};
use arca_core::types::BlobId;

use crate::encryption::format::{self, HEADER_SIZE};
use crate::encryption::keys::{generate_nonce_prefix, make_aead_key};
use crate::encryption::stream::{
    chunk_disk_offset, decrypt_range, DecryptingStream, EncryptingStream,
};
use crate::fs::FsBlobStore;

/// SSE-C blob store. Wraps `FsBlobStore` and encrypts/decrypts using a
/// customer-provided key passed to each operation.
pub struct SsecBlobStore {
    inner: FsBlobStore,
}

impl SsecBlobStore {
    pub fn new(inner: FsBlobStore) -> Self {
        Self { inner }
    }

    /// Returns a reference to the inner `FsBlobStore`.
    pub fn inner(&self) -> &FsBlobStore {
        &self.inner
    }

    /// Writes a blob encrypted with the customer-provided key.
    ///
    /// Returns `(BlobPutResult, nonce_prefix)`. The nonce_prefix must be stored
    /// in the sidecar for later decryption.
    pub async fn put_with_key(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
        customer_key: &[u8; 32],
    ) -> Result<(BlobPutResult, [u8; 4]), ArcaError> {
        let nonce_prefix = generate_nonce_prefix()
            .map_err(|e| ArcaError::Internal(format!("generate nonce prefix: {e}")))?;

        let key = make_aead_key(customer_key)
            .map_err(|e| ArcaError::Internal(format!("create AEAD key: {e}")))?;

        let (enc_stream, plaintext_stats) =
            EncryptingStream::new(stream, key, nonce_prefix, format::DEFAULT_CHUNK_SIZE);

        // Write encrypted data to the inner store.
        let _ciphertext_result = self.inner.put(blob_id, Box::pin(enc_stream)).await?;

        // Get plaintext stats.
        let stats = plaintext_stats.lock().unwrap();
        let md5_bytes = stats
            .md5
            .ok_or_else(|| ArcaError::Internal("plaintext MD5 not computed".into()))?;

        let result = BlobPutResult {
            size: stats.size,
            etag: hex::encode(md5_bytes),
            encryption: None,
        };

        Ok((result, nonce_prefix))
    }

    /// Reads a blob encrypted with SSE-C using the customer-provided key (full read).
    pub async fn get_with_key(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
        customer_key: &[u8; 32],
        nonce_prefix: &[u8; 4],
        plaintext_size: u64,
    ) -> Result<BlobGetResult, ArcaError> {
        match range {
            None => {
                self.get_full(blob_id, customer_key, nonce_prefix, plaintext_size)
                    .await
            }
            Some(r) => {
                self.get_range(blob_id, customer_key, nonce_prefix, plaintext_size, r)
                    .await
            }
        }
    }

    async fn get_full(
        &self,
        blob_id: &BlobId,
        customer_key: &[u8; 32],
        nonce_prefix: &[u8; 4],
        plaintext_size: u64,
    ) -> Result<BlobGetResult, ArcaError> {
        let blob_path = self.inner.blob_path(blob_id);
        let mut file = fs::File::open(&blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("open blob: {e}")))?;

        // Read and validate the header.
        let mut header_buf = [0u8; HEADER_SIZE];
        file.read_exact(&mut header_buf)
            .await
            .map_err(|e| ArcaError::Internal(format!("read header: {e}")))?;
        let _chunk_size = format::parse_header(&header_buf)
            .map_err(|e| ArcaError::Internal(format!("parse header: {e}")))?;

        // Create a fresh AEAD key (LessSafeKey doesn't impl Clone).
        let key = make_aead_key(customer_key)
            .map_err(|e| ArcaError::Internal(format!("create AEAD key: {e}")))?;
        let reader = tokio_util::io::ReaderStream::new(file);
        let cipher_stream: ByteStream = Box::pin(reader);
        let dec_stream = DecryptingStream::new(cipher_stream, key, *nonce_prefix);

        Ok(BlobGetResult {
            stream: Box::pin(dec_stream),
            content_length: plaintext_size,
        })
    }

    async fn get_range(
        &self,
        blob_id: &BlobId,
        customer_key: &[u8; 32],
        nonce_prefix: &[u8; 4],
        plaintext_size: u64,
        range: ByteRange,
    ) -> Result<BlobGetResult, ArcaError> {
        let blob_path = self.inner.blob_path(blob_id);
        let mut file = fs::File::open(&blob_path)
            .await
            .map_err(|e| ArcaError::Internal(format!("open blob: {e}")))?;

        // Read header to get chunk_size.
        let mut header_buf = [0u8; HEADER_SIZE];
        file.read_exact(&mut header_buf)
            .await
            .map_err(|e| ArcaError::Internal(format!("read header: {e}")))?;
        let chunk_size = format::parse_header(&header_buf)
            .map_err(|e| ArcaError::Internal(format!("parse header: {e}")))?;

        let range_start = range.start;
        let range_end = range
            .end
            .map(|e| e.min(plaintext_size - 1))
            .unwrap_or(plaintext_size - 1);
        let content_length = range_end - range_start + 1;

        // Calculate which chunks overlap.
        let first_chunk = range_start / chunk_size as u64;
        let last_chunk = range_end / chunk_size as u64;

        // Read the encrypted chunk data from disk.
        let disk_start = chunk_disk_offset(first_chunk, chunk_size);
        let file_size = file
            .metadata()
            .await
            .map_err(|e| ArcaError::Internal(format!("file metadata: {e}")))?
            .len();

        let disk_end =
            if last_chunk < (plaintext_size + chunk_size as u64 - 1) / chunk_size as u64 - 1 {
                chunk_disk_offset(last_chunk + 1, chunk_size)
            } else {
                file_size
            };

        let read_len = (disk_end - disk_start) as usize;
        file.seek(std::io::SeekFrom::Start(disk_start))
            .await
            .map_err(|e| ArcaError::Internal(format!("seek blob: {e}")))?;

        let mut chunk_data = vec![0u8; read_len];
        file.read_exact(&mut chunk_data)
            .await
            .map_err(|e| ArcaError::Internal(format!("read chunks: {e}")))?;

        let key = make_aead_key(customer_key)
            .map_err(|e| ArcaError::Internal(format!("create AEAD key: {e}")))?;
        let plaintext = decrypt_range(
            &key,
            nonce_prefix,
            chunk_size,
            &chunk_data,
            first_chunk,
            range_start,
            range_end,
        )
        .map_err(|e| ArcaError::Internal(format!("decrypt range: {e}")))?;

        let stream: ByteStream = Box::pin(tokio_stream::iter(vec![Ok(Bytes::from(plaintext))]));

        Ok(BlobGetResult {
            stream,
            content_length,
        })
    }

    /// Delegates delete to the inner store (no key needed).
    pub async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        self.inner.delete(blob_id).await
    }

    /// Delegates sidecar write to the inner store.
    pub async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), ArcaError> {
        self.inner.write_sidecar(blob_id, meta).await
    }
}

#[async_trait::async_trait]
impl SsecBlobOps for SsecBlobStore {
    async fn put_with_key(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
        customer_key: &[u8; 32],
    ) -> Result<(BlobPutResult, [u8; 4]), ArcaError> {
        SsecBlobStore::put_with_key(self, blob_id, stream, customer_key).await
    }

    async fn get_with_key(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
        customer_key: &[u8; 32],
        nonce_prefix: &[u8; 4],
        plaintext_size: u64,
    ) -> Result<BlobGetResult, ArcaError> {
        SsecBlobStore::get_with_key(self, blob_id, range, customer_key, nonce_prefix, plaintext_size)
            .await
    }

    async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        SsecBlobStore::delete(self, blob_id).await
    }

    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), ArcaError> {
        SsecBlobStore::write_sidecar(self, blob_id, meta).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio_stream::iter as stream_iter;
    use tokio_stream::StreamExt;

    fn bytes_to_stream(data: &[u8]) -> ByteStream {
        let chunks = vec![Ok(Bytes::copy_from_slice(data))];
        Box::pin(stream_iter(chunks))
    }

    async fn collect_stream(stream: ByteStream) -> Vec<u8> {
        let mut stream = std::pin::pin!(stream);
        let mut buf = Vec::new();
        while let Some(chunk) = stream.as_mut().next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }

    async fn test_store() -> (SsecBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let fs_store = FsBlobStore::new(dir.path().join("blobs"), 2).await.unwrap();
        let ssec_store = SsecBlobStore::new(fs_store);
        (ssec_store, dir)
    }

    fn test_key() -> [u8; 32] {
        [0xABu8; 32]
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"SSE-C encrypted data";
        let key = test_key();

        let (put_result, nonce_prefix) = store
            .put_with_key(&blob_id, bytes_to_stream(data), &key)
            .await
            .unwrap();

        assert_eq!(put_result.size, data.len() as u64);
        assert!(put_result.encryption.is_none()); // SSE-C doesn't use BlobEncryptionInfo

        // Write a sidecar so the test is complete.
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "test.txt".into(),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            version_id: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        // Full read.
        let get_result = store
            .get_with_key(&blob_id, None, &key, &nonce_prefix, put_result.size)
            .await
            .unwrap();
        assert_eq!(get_result.content_length, data.len() as u64);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, data);
    }

    #[tokio::test]
    async fn wrong_key_fails() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"SSE-C encrypted data";
        let key = test_key();
        let wrong_key = [0xCDu8; 32];

        let (put_result, nonce_prefix) = store
            .put_with_key(&blob_id, bytes_to_stream(data), &key)
            .await
            .unwrap();

        // get_with_key succeeds (returns a stream), but reading the stream fails
        // because GCM auth tag verification fails during decryption.
        let get_result = store
            .get_with_key(&blob_id, None, &wrong_key, &nonce_prefix, put_result.size)
            .await
            .unwrap();

        let mut stream = std::pin::pin!(get_result.stream);
        let mut had_error = false;
        while let Some(chunk) = stream.as_mut().next().await {
            if chunk.is_err() {
                had_error = true;
                break;
            }
        }
        assert!(had_error, "expected decryption error with wrong key");
    }

    #[tokio::test]
    async fn range_read() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"0123456789abcdefghijklmnopqrstuvwxyz";
        let key = test_key();

        let (put_result, nonce_prefix) = store
            .put_with_key(&blob_id, bytes_to_stream(data), &key)
            .await
            .unwrap();

        let range = ByteRange {
            start: 10,
            end: Some(19),
        };
        let get_result = store
            .get_with_key(
                &blob_id,
                Some(range),
                &key,
                &nonce_prefix,
                put_result.size,
            )
            .await
            .unwrap();
        assert_eq!(get_result.content_length, 10);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, b"abcdefghij");
    }

    #[tokio::test]
    async fn empty_object() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let key = test_key();

        let (put_result, nonce_prefix) = store
            .put_with_key(&blob_id, bytes_to_stream(b""), &key)
            .await
            .unwrap();
        assert_eq!(put_result.size, 0);

        let get_result = store
            .get_with_key(&blob_id, None, &key, &nonce_prefix, 0)
            .await
            .unwrap();
        assert_eq!(get_result.content_length, 0);
        let body = collect_stream(get_result.stream).await;
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn delete_no_key_needed() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let key = test_key();

        let (_put_result, _nonce_prefix) = store
            .put_with_key(&blob_id, bytes_to_stream(b"delete me"), &key)
            .await
            .unwrap();

        // Delete doesn't need the customer key.
        store.delete(&blob_id).await.unwrap();

        // Verify blob is gone.
        let result = store.get_with_key(&blob_id, None, &key, &[0; 4], 0).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn etag_is_plaintext_md5() {
        use md5::{Digest, Md5};

        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"check the etag";
        let key = test_key();

        let (put_result, _nonce_prefix) = store
            .put_with_key(&blob_id, bytes_to_stream(data), &key)
            .await
            .unwrap();

        let expected_etag = hex::encode(Md5::digest(data));
        assert_eq!(put_result.etag, expected_etag);
    }
}
