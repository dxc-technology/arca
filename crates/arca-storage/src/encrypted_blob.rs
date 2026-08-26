//! `BlobStore` wrapper that transparently encrypts/decrypts objects.
//!
//! Wraps `FsBlobStore` to add AES-256-GCM envelope encryption.
//! Encrypted and unencrypted blobs coexist transparently (mixed-mode).

use std::sync::Arc;

use base64::Engine;
use bytes::Bytes;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use arca_core::error::ArcaError;
use arca_core::store::{
    BlobEncryptionInfo, BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream,
    CompositePart, SidecarMeta,
};
use arca_core::types::BlobId;
use md5::{Digest, Md5};

use crate::encryption::format::{self, HEADER_SIZE};
use crate::encryption::keys::{
    generate_dek, generate_nonce_prefix, make_aead_key, MasterKey,
};
use crate::encryption::stream::{
    chunk_disk_offset, decrypt_range, DecryptingStream, EncryptingStream,
};
use crate::fs::FsBlobStore;

/// Encrypting blob store wrapper.
///
/// Implements `BlobStore` by delegating to an inner `FsBlobStore`.
/// On `put()`, wraps the input stream with encryption and returns
/// plaintext MD5/size. On `get()`, reads the sidecar to detect
/// encrypted blobs and transparently decrypts them.
pub struct EncryptingBlobStore {
    inner: FsBlobStore,
    master_key: Arc<MasterKey>,
}

impl EncryptingBlobStore {
    pub fn new(inner: FsBlobStore, master_key: Arc<MasterKey>) -> Self {
        Self { inner, master_key }
    }

    /// Reads and parses the sidecar for a blob.
    /// Returns None if the sidecar doesn't exist.
    async fn read_sidecar(&self, blob_id: &BlobId) -> Result<Option<SidecarMeta>, ArcaError> {
        let sidecar_path = self.inner.sidecar_path(blob_id);
        match fs::read_to_string(&sidecar_path).await {
            Ok(json) => {
                let meta: SidecarMeta = serde_json::from_str(&json)
                    .map_err(|e| ArcaError::Internal(format!("parse sidecar: {e}")))?;
                Ok(Some(meta))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ArcaError::Internal(format!("read sidecar: {e}"))),
        }
    }

    /// Decrypts the DEK from sidecar encryption info.
    fn unwrap_dek(&self, enc_info: &BlobEncryptionInfo) -> Result<Vec<u8>, ArcaError> {
        let b64 = &base64::engine::general_purpose::STANDARD;
        let encrypted_dek = b64
            .decode(&enc_info.encrypted_dek)
            .map_err(|e| ArcaError::Internal(format!("decode encrypted_dek: {e}")))?;
        let dek_nonce = b64
            .decode(&enc_info.dek_nonce)
            .map_err(|e| ArcaError::Internal(format!("decode dek_nonce: {e}")))?;
        self.master_key
            .unwrap_dek(&encrypted_dek, &dek_nonce)
            .map_err(|_| ArcaError::DecryptionFailed(
                "The object was encrypted with a different master key and cannot be decrypted with the current key".to_string(),
            ))
    }

    /// Handles `get()` for an encrypted blob (full read).
    async fn get_encrypted_full(
        &self,
        blob_id: &BlobId,
        enc_info: &BlobEncryptionInfo,
        plaintext_size: u64,
    ) -> Result<BlobGetResult, ArcaError> {
        let dek = self.unwrap_dek(enc_info)?;
        let b64 = &base64::engine::general_purpose::STANDARD;
        let nonce_prefix_bytes = b64
            .decode(&enc_info.nonce_prefix)
            .map_err(|e| ArcaError::Internal(format!("decode nonce_prefix: {e}")))?;
        let mut nonce_prefix = [0u8; 4];
        if nonce_prefix_bytes.len() != 4 {
            return Err(ArcaError::Internal("nonce_prefix must be 4 bytes".into()));
        }
        nonce_prefix.copy_from_slice(&nonce_prefix_bytes);

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

        // Stream the rest through DecryptingStream.
        let key = make_aead_key(&dek)
            .map_err(|e| ArcaError::Internal(format!("create AEAD key: {e}")))?;
        let reader = tokio_util::io::ReaderStream::new(file);
        let cipher_stream: ByteStream = Box::pin(reader);
        let dec_stream = DecryptingStream::new(cipher_stream, key, nonce_prefix);

        Ok(BlobGetResult {
            stream: Box::pin(dec_stream),
            content_length: plaintext_size,
        })
    }

    /// Handles `get()` for an encrypted blob with byte range.
    async fn get_encrypted_range(
        &self,
        blob_id: &BlobId,
        enc_info: &BlobEncryptionInfo,
        plaintext_size: u64,
        range: ByteRange,
    ) -> Result<BlobGetResult, ArcaError> {
        let dek = self.unwrap_dek(enc_info)?;
        let b64 = &base64::engine::general_purpose::STANDARD;
        let nonce_prefix_bytes = b64
            .decode(&enc_info.nonce_prefix)
            .map_err(|e| ArcaError::Internal(format!("decode nonce_prefix: {e}")))?;
        let mut nonce_prefix = [0u8; 4];
        if nonce_prefix_bytes.len() != 4 {
            return Err(ArcaError::Internal("nonce_prefix must be 4 bytes".into()));
        }
        nonce_prefix.copy_from_slice(&nonce_prefix_bytes);

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
        // For the end: we need to read through last_chunk. But the last chunk
        // may be partial, so we can't just use on_disk_chunk_size for it.
        // Read to the end of the file from disk_start is simplest and correct.
        let file_size = file
            .metadata()
            .await
            .map_err(|e| ArcaError::Internal(format!("file metadata: {e}")))?
            .len();

        // For efficiency, compute the expected end position.
        // If last_chunk is not the very last chunk, we know its size.
        let disk_end = if last_chunk < (plaintext_size + chunk_size as u64 - 1) / chunk_size as u64 - 1 {
            // Not the last chunk — all chunks up to last_chunk are full.
            chunk_disk_offset(last_chunk + 1, chunk_size)
        } else {
            file_size // Read to end of file for the last partial chunk.
        };

        let read_len = (disk_end - disk_start) as usize;
        file.seek(std::io::SeekFrom::Start(disk_start))
            .await
            .map_err(|e| ArcaError::Internal(format!("seek blob: {e}")))?;

        let mut chunk_data = vec![0u8; read_len];
        file.read_exact(&mut chunk_data)
            .await
            .map_err(|e| ArcaError::Internal(format!("read chunks: {e}")))?;

        let key = make_aead_key(&dek)
            .map_err(|e| ArcaError::Internal(format!("create AEAD key: {e}")))?;
        let plaintext = decrypt_range(
            &key,
            &nonce_prefix,
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

    /// Streams plaintext from a composite blob (the result of an encrypted
    /// `CompleteMultipartUpload`). The `parts` are still encrypted on disk
    /// under their own per-part DEKs; we decrypt only the chunks that
    /// overlap the requested range.
    async fn get_composite(
        &self,
        parts: &[CompositePart],
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        let total: u64 = parts.iter().map(|p| p.plaintext_size).sum();

        // Resolve absolute plaintext range [range_start, range_end] inclusive.
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

        // Walk parts in order, eagerly building the per-part read futures.
        // Each part contributes its [sub_start, sub_end] inclusive sub-range.
        let mut combined: ByteStream = Box::pin(tokio_stream::empty());
        let mut cum_start: u64 = 0;
        for part in parts {
            let part_size = part.plaintext_size;
            let part_end_excl = cum_start + part_size;
            // Skip parts entirely outside the requested range.
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

            let part_result = match part.encryption.as_ref() {
                Some(enc) if sub_start == 0 && sub_end == part_size - 1 => {
                    self.get_encrypted_full(&part.blob_id, enc, part_size).await?
                }
                Some(enc) => {
                    self.get_encrypted_range(
                        &part.blob_id,
                        enc,
                        part_size,
                        ByteRange {
                            start: sub_start,
                            end: Some(sub_end),
                        },
                    )
                    .await?
                }
                None => {
                    // Plain part inside an encrypted composite shouldn't happen
                    // in normal flow (concat fallback handles mixed parts), but
                    // be defensive and just delegate to the inner store.
                    let r = if sub_start == 0 && sub_end == part_size - 1 {
                        None
                    } else {
                        Some(ByteRange {
                            start: sub_start,
                            end: Some(sub_end),
                        })
                    };
                    self.inner.get(&part.blob_id, r).await?
                }
            };
            combined = Box::pin(tokio_stream::StreamExt::chain(combined, part_result.stream));
            cum_start = part_end_excl;
        }

        Ok(BlobGetResult {
            stream: combined,
            content_length,
        })
    }
}

#[async_trait::async_trait]
impl BlobStore for EncryptingBlobStore {
    async fn put(
        &self,
        blob_id: &BlobId,
        stream: ByteStream,
    ) -> Result<BlobPutResult, ArcaError> {
        let b64 = &base64::engine::general_purpose::STANDARD;

        // Generate per-object DEK and nonce prefix.
        let dek = generate_dek()
            .map_err(|e| ArcaError::Internal(format!("generate DEK: {e}")))?;
        let nonce_prefix = generate_nonce_prefix()
            .map_err(|e| ArcaError::Internal(format!("generate nonce prefix: {e}")))?;

        // Wrap DEK with master key.
        let (encrypted_dek, dek_nonce) = self
            .master_key
            .wrap_dek(&dek)
            .map_err(|e| ArcaError::Internal(format!("wrap DEK: {e}")))?;

        // Create the encrypting stream.
        let key = make_aead_key(&dek)
            .map_err(|e| ArcaError::Internal(format!("create AEAD key: {e}")))?;
        let (enc_stream, plaintext_stats) = EncryptingStream::new(
            stream,
            key,
            nonce_prefix,
            format::DEFAULT_CHUNK_SIZE,
        );

        // Write encrypted data to the inner store.
        let _ciphertext_result = self.inner.put(blob_id, Box::pin(enc_stream)).await?;

        // Get plaintext stats (populated after stream was fully consumed).
        let stats = plaintext_stats.lock().unwrap();
        let md5_bytes = stats
            .md5
            .ok_or_else(|| ArcaError::Internal("plaintext MD5 not computed".into()))?;

        Ok(BlobPutResult {
            size: stats.size,
            etag: hex::encode(md5_bytes),
            encryption: Some(BlobEncryptionInfo {
                algorithm: "AES256".to_string(),
                encrypted_dek: b64.encode(&encrypted_dek),
                dek_nonce: b64.encode(&dek_nonce),
                nonce_prefix: b64.encode(nonce_prefix),
                key_id: self.master_key.key_id().to_string(),
            }),
            compression: None,
            composite_parts: None,
        })
    }

    async fn get(
        &self,
        blob_id: &BlobId,
        range: Option<ByteRange>,
    ) -> Result<BlobGetResult, ArcaError> {
        // Read sidecar to check if the blob is encrypted.
        let sidecar = self.read_sidecar(blob_id).await?;

        // Composite blob (result of an encrypted CompleteMultipartUpload):
        // no on-disk file at this blob_id, parts live separately and are
        // streamed through.
        if let Some(parts) = sidecar.as_ref().and_then(|s| s.composite.as_ref()) {
            return self.get_composite(parts, range).await;
        }

        let enc_info = sidecar.as_ref().and_then(|s| s.encryption.as_ref());

        match enc_info {
            None => {
                // Unencrypted blob — delegate directly.
                self.inner.get(blob_id, range).await
            }
            Some(enc_info) => {
                let plaintext_size = sidecar.as_ref().unwrap().size;
                match range {
                    None => {
                        self.get_encrypted_full(blob_id, enc_info, plaintext_size)
                            .await
                    }
                    Some(range) => {
                        self.get_encrypted_range(blob_id, enc_info, plaintext_size, range)
                            .await
                    }
                }
            }
        }
    }

    async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        // Composite blob: cascade-delete every part it points to before
        // removing the composite sidecar itself. Each part's blob_id is a
        // standalone encrypted blob with its own sidecar; `inner.delete`
        // takes care of both the file and its sidecar.
        if let Ok(Some(sidecar)) = self.read_sidecar(blob_id).await {
            if let Some(parts) = sidecar.composite {
                for p in &parts {
                    // Best-effort: log and continue if a single part is missing.
                    if let Err(e) = self.inner.delete(&p.blob_id).await {
                        tracing::warn!(
                            error = %e,
                            blob_id = %p.blob_id.0,
                            "failed to delete composite part during cascade"
                        );
                    }
                }
            }
        }
        self.inner.delete(blob_id).await
    }

    async fn delete_assembled(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
        // No cascade into composite parts — the caller wants to discard only
        // the assembled blob itself.
        self.inner.delete_assembled(blob_id).await
    }

    async fn write_sidecar(
        &self,
        blob_id: &BlobId,
        meta: &SidecarMeta,
    ) -> Result<(), ArcaError> {
        self.inner.write_sidecar(blob_id, meta).await
    }

    /// Override concat to avoid the default decrypt+re-encrypt path.
    ///
    /// Reads each part's sidecar to capture its DEK, nonce_prefix, and
    /// plaintext MD5. Returns a `BlobPutResult` carrying `composite_parts`
    /// so the caller writes a sidecar that points at the still-on-disk
    /// part files. CompleteMultipartUpload becomes O(N) sidecar reads
    /// instead of O(N * part_size) AEAD work.
    ///
    /// Falls back to the default `get + put` impl when any part has
    /// compression metadata (the composite path doesn't carry per-part
    /// compression info on the final sidecar yet).
    async fn concat(
        &self,
        part_blob_ids: &[BlobId],
        output_blob_id: &BlobId,
    ) -> Result<BlobPutResult, ArcaError> {
        // Quick scan: if any part is compressed or unencrypted, fall back
        // to the safe (slow) decrypt+re-encrypt path.
        let mut composite_parts: Vec<CompositePart> = Vec::with_capacity(part_blob_ids.len());
        let mut total_size: u64 = 0;
        let mut md5_concat: Vec<u8> = Vec::with_capacity(part_blob_ids.len() * 16);
        let mut needs_fallback = false;

        for blob_id in part_blob_ids {
            let sidecar = self
                .read_sidecar(blob_id)
                .await?
                .ok_or_else(|| {
                    ArcaError::Internal(format!(
                        "missing sidecar for multipart part {}",
                        blob_id.0
                    ))
                })?;

            let enc_info = match sidecar.encryption.clone() {
                Some(e) => e,
                None => {
                    needs_fallback = true;
                    break;
                }
            };
            if sidecar.compression.is_some() {
                needs_fallback = true;
                break;
            }

            let md5_bytes = hex::decode(&sidecar.etag).map_err(|e| {
                ArcaError::Internal(format!("part {} etag is not hex: {e}", blob_id.0))
            })?;
            if md5_bytes.len() != 16 {
                return Err(ArcaError::Internal(format!(
                    "part {} has malformed etag (not 16 bytes)",
                    blob_id.0
                )));
            }
            md5_concat.extend_from_slice(&md5_bytes);
            total_size += sidecar.size;

            composite_parts.push(CompositePart {
                blob_id: blob_id.clone(),
                plaintext_size: sidecar.size,
                plaintext_etag: sidecar.etag,
                encryption: Some(enc_info),
            });
        }

        if needs_fallback {
            tracing::debug!(
                "EncryptingBlobStore::concat falling back to decrypt+re-encrypt \
                 (one or more parts is unencrypted or compressed)"
            );
            // Replicate the trait default — get each, chain, put.
            let mut combined: ByteStream = Box::pin(tokio_stream::empty());
            for blob_id in part_blob_ids {
                let result = self.get(blob_id, None).await?;
                combined = Box::pin(tokio_stream::StreamExt::chain(combined, result.stream));
            }
            return self.put(output_blob_id, combined).await;
        }

        // Composite ETag (without S3 multipart -N suffix; the handler adds
        // that). The handler currently overrides this anyway, but we set a
        // sensible value for consumers that read BlobPutResult.etag.
        let composite_md5 = Md5::digest(&md5_concat);

        Ok(BlobPutResult {
            size: total_size,
            etag: hex::encode(composite_md5),
            // Marker encryption info: signals "encrypted" but the per-part
            // DEKs/nonces live in `composite_parts`. The fields are blank
            // because the composite has no top-level DEK.
            encryption: Some(BlobEncryptionInfo {
                algorithm: "AES256".to_string(),
                encrypted_dek: String::new(),
                dek_nonce: String::new(),
                nonce_prefix: String::new(),
                key_id: self.master_key.key_id().to_string(),
            }),
            compression: None,
            composite_parts: Some(composite_parts),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use md5::{Digest, Md5};
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

    fn test_master_key() -> Arc<MasterKey> {
        Arc::new(MasterKey::from_bytes(&[0x42u8; 32]).unwrap())
    }

    async fn test_store() -> (EncryptingBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let fs_store = FsBlobStore::new(dir.path().join("blobs"), 2).await.unwrap();
        let enc_store = EncryptingBlobStore::new(fs_store, test_master_key());
        (enc_store, dir)
    }

    #[tokio::test]
    async fn put_returns_plaintext_etag_and_size() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"hello encrypted world";

        let result = store.put(&blob_id, bytes_to_stream(data)).await.unwrap();

        // Size should be plaintext size.
        assert_eq!(result.size, data.len() as u64);

        // ETag should be MD5 of plaintext.
        let expected_etag = hex::encode(Md5::digest(data));
        assert_eq!(result.etag, expected_etag);

        // Encryption info should be populated.
        let enc = result.encryption.unwrap();
        assert_eq!(enc.algorithm, "AES256");
        assert!(!enc.encrypted_dek.is_empty());
        assert!(!enc.dek_nonce.is_empty());
        assert!(!enc.nonce_prefix.is_empty());
        assert_eq!(enc.key_id.len(), 8);
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"roundtrip through encrypted blob store";

        let put_result = store.put(&blob_id, bytes_to_stream(data)).await.unwrap();

        // Write sidecar so get() can find encryption info.
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "test.txt".into(),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: put_result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        // Full read.
        let get_result = store.get(&blob_id, None).await.unwrap();
        assert_eq!(get_result.content_length, data.len() as u64);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, data);
    }

    #[tokio::test]
    async fn put_get_range_read() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"0123456789abcdefghijklmnopqrstuvwxyz";

        let put_result = store.put(&blob_id, bytes_to_stream(data)).await.unwrap();

        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "test.txt".into(),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: put_result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        // Range read: bytes 10..=19 -> "abcdefghij"
        let range = ByteRange {
            start: 10,
            end: Some(19),
        };
        let get_result = store.get(&blob_id, Some(range)).await.unwrap();
        assert_eq!(get_result.content_length, 10);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, b"abcdefghij");
    }

    #[tokio::test]
    async fn unencrypted_blob_passthrough() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        let data = b"plaintext data";

        // Write directly via inner store (bypassing encryption).
        let put_result = store.inner.put(&blob_id, bytes_to_stream(data)).await.unwrap();

        // Write sidecar WITHOUT encryption info.
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "plain.txt".into(),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        // Get via encrypting store — should passthrough.
        let get_result = store.get(&blob_id, None).await.unwrap();
        assert_eq!(get_result.content_length, data.len() as u64);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, data);
    }

    #[tokio::test]
    async fn put_get_empty_blob() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();

        let put_result = store.put(&blob_id, bytes_to_stream(b"")).await.unwrap();
        assert_eq!(put_result.size, 0);

        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "empty.txt".into(),
            size: 0,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: put_result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        let get_result = store.get(&blob_id, None).await.unwrap();
        assert_eq!(get_result.content_length, 0);
        let body = collect_stream(get_result.stream).await;
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn put_get_large_blob() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        // 200 KiB — spans multiple 64 KiB chunks.
        let data: Vec<u8> = (0..200_000).map(|i| (i % 256) as u8).collect();

        let put_result = store.put(&blob_id, bytes_to_stream(&data)).await.unwrap();
        assert_eq!(put_result.size, 200_000);

        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "large.bin".into(),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: put_result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        let get_result = store.get(&blob_id, None).await.unwrap();
        assert_eq!(get_result.content_length, 200_000);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, data);
    }

    #[tokio::test]
    async fn range_read_cross_chunk_boundary() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();
        // 128 KiB — exactly 2 full 64 KiB chunks.
        let data: Vec<u8> = (0..131_072).map(|i| (i % 256) as u8).collect();

        let put_result = store.put(&blob_id, bytes_to_stream(&data)).await.unwrap();

        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "cross.bin".into(),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: put_result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        // Range spanning chunk boundary: bytes 65530..=65541
        let range = ByteRange {
            start: 65530,
            end: Some(65541),
        };
        let get_result = store.get(&blob_id, Some(range)).await.unwrap();
        assert_eq!(get_result.content_length, 12);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, &data[65530..=65541]);
    }

    #[tokio::test]
    async fn delete_encrypted_blob() {
        let (store, _dir) = test_store().await;
        let blob_id = BlobId::new();

        let put_result = store.put(&blob_id, bytes_to_stream(b"delete me")).await.unwrap();

        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "del.txt".into(),
            size: put_result.size,
            etag: put_result.etag,
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: put_result.encryption,
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&blob_id, &sidecar).await.unwrap();

        store.delete(&blob_id).await.unwrap();

        // Get should fail.
        assert!(store.get(&blob_id, None).await.is_err());
    }

    // ----- Composite (multipart concat) tests -----

    /// Helper: write `n_parts` of `part_size` random-ish bytes as encrypted
    /// blobs. Returns the part blob ids and the concatenated plaintext that
    /// the composite blob should reproduce.
    async fn make_encrypted_parts(
        store: &EncryptingBlobStore,
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
            // Each part needs its own sidecar so concat can read encryption info.
            let sidecar = SidecarMeta {
                bucket: "test".into(),
                key: format!("part-{i}"),
                size: put_result.size,
                etag: put_result.etag.clone(),
                content_type: None,
                last_modified: "2026-01-01T00:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: put_result.encryption.clone(),
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
    async fn concat_composite_returns_parts() {
        let (store, _dir) = test_store().await;
        let (part_ids, _full) = make_encrypted_parts(&store, 3, 64).await;
        let output_id = BlobId::new();

        let result = store.concat(&part_ids, &output_id).await.unwrap();

        // Composite path is taken — composite_parts populated, no on-disk file.
        let parts = result.composite_parts.expect("composite path should engage");
        assert_eq!(parts.len(), 3);
        assert_eq!(result.size, 3 * 64);
        // Each part has matching blob_id and plaintext_size.
        for (i, p) in parts.iter().enumerate() {
            assert_eq!(p.blob_id, part_ids[i]);
            assert_eq!(p.plaintext_size, 64);
            assert!(!p.encryption.as_ref().unwrap().encrypted_dek.is_empty());
        }
        // The output blob file should NOT exist (composite is sidecar-only).
        let output_path = store.inner.blob_path(&output_id);
        assert!(!output_path.exists());
    }

    #[tokio::test]
    async fn concat_composite_get_full_roundtrip() {
        let (store, _dir) = test_store().await;
        let (part_ids, full) = make_encrypted_parts(&store, 3, 256).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();

        // Caller writes the composite sidecar.
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "composite.bin".into(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        // Full read of the composite returns concatenated plaintext.
        let get_result = store.get(&output_id, None).await.unwrap();
        assert_eq!(get_result.content_length, full.len() as u64);
        let body = collect_stream(get_result.stream).await;
        assert_eq!(body, full);
    }

    #[tokio::test]
    async fn concat_composite_get_range_within_single_part() {
        let (store, _dir) = test_store().await;
        let (part_ids, full) = make_encrypted_parts(&store, 3, 100).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "c.bin".into(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        // Range fully inside part 1: bytes 110..=130 (part 1 covers [100, 200)).
        let range = ByteRange { start: 110, end: Some(130) };
        let g = store.get(&output_id, Some(range)).await.unwrap();
        assert_eq!(g.content_length, 21);
        let body = collect_stream(g.stream).await;
        assert_eq!(body, &full[110..=130]);
    }

    #[tokio::test]
    async fn concat_composite_get_range_cross_two_parts() {
        let (store, _dir) = test_store().await;
        let (part_ids, full) = make_encrypted_parts(&store, 3, 100).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "c.bin".into(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        // Range crosses part 0 / part 1 boundary.
        let range = ByteRange { start: 80, end: Some(150) };
        let g = store.get(&output_id, Some(range)).await.unwrap();
        assert_eq!(g.content_length, 71);
        let body = collect_stream(g.stream).await;
        assert_eq!(body, &full[80..=150]);
    }

    #[tokio::test]
    async fn concat_composite_get_range_spans_three_parts() {
        let (store, _dir) = test_store().await;
        let (part_ids, full) = make_encrypted_parts(&store, 3, 100).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "c.bin".into(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        // Range covers entire part 1 plus tails of part 0 and part 2.
        let range = ByteRange { start: 50, end: Some(250) };
        let g = store.get(&output_id, Some(range)).await.unwrap();
        assert_eq!(g.content_length, 201);
        let body = collect_stream(g.stream).await;
        assert_eq!(body, &full[50..=250]);
    }

    #[tokio::test]
    async fn concat_composite_delete_cascades_to_parts() {
        let (store, _dir) = test_store().await;
        let (part_ids, _full) = make_encrypted_parts(&store, 2, 64).await;
        let output_id = BlobId::new();
        let result = store.concat(&part_ids, &output_id).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "c.bin".into(),
            size: result.size,
            etag: result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: result.encryption.clone(),
            compression: None,
            version_id: None,
            composite: result.composite_parts.clone(),
        };
        store.write_sidecar(&output_id, &sidecar).await.unwrap();

        // Sanity: parts on disk before delete.
        for id in &part_ids {
            assert!(store.inner.blob_path(id).exists());
        }

        store.delete(&output_id).await.unwrap();

        // After delete, all part files are gone.
        for id in &part_ids {
            assert!(
                !store.inner.blob_path(id).exists(),
                "part {} should have been deleted",
                id.0
            );
            assert!(store.get(id, None).await.is_err());
        }
        // Composite get also fails.
        assert!(store.get(&output_id, None).await.is_err());
    }

    #[tokio::test]
    async fn concat_falls_back_when_part_unencrypted() {
        // If a part has no encryption info, the composite path is unsafe;
        // we expect a fallback to decrypt+re-encrypt.
        let (store, _dir) = test_store().await;
        let part_id = BlobId::new();
        let part_data = b"plain";
        // Bypass encryption by writing through inner.
        let inner_result = store.inner.put(&part_id, bytes_to_stream(part_data)).await.unwrap();
        let sidecar = SidecarMeta {
            bucket: "test".into(),
            key: "p".into(),
            size: inner_result.size,
            etag: inner_result.etag.clone(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None, // <-- unencrypted
            compression: None,
            version_id: None,
            composite: None,
        };
        store.write_sidecar(&part_id, &sidecar).await.unwrap();

        let output_id = BlobId::new();
        let result = store.concat(&[part_id], &output_id).await.unwrap();
        // Fallback path: composite_parts is None, an actual blob was written.
        assert!(result.composite_parts.is_none());
        assert!(store.inner.blob_path(&output_id).exists());
    }
}
