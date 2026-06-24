//! Shared re-encryption engine: converts blob files between plaintext and
//! SSE-S3 (AES-256-GCM) on disk.
//!
//! This is the single core reused by both surfaces of the maintenance
//! re-encryption job (Phase 30):
//!
//! * the **console-driven worker** (hot copy-on-write and maintenance-mode
//!   in-place rewrites), and
//! * the **offline CLI escape hatch** (`arca encrypt-existing` /
//!   `decrypt-existing`) for disaster recovery when the server is down.
//!
//! It is built on the very same `EncryptingStream` / `DecryptingStream` +
//! DEK-wrap primitives as [`crate::encrypted_blob::EncryptingBlobStore`], so a
//! blob re-encrypted here is byte-for-byte identical to one written through the
//! live encrypted path and reads back transparently (same `AENC` framing, same
//! per-object DEK wrapped by the master KEK, same chunk size).

use std::path::Path;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_stream::StreamExt as _;
use tokio_util::io::ReaderStream;

use arca_core::error::ArcaError;
use arca_core::store::blob::{BlobEncryptionInfo, ByteStream, SidecarMeta};

use crate::encryption::format;
use crate::encryption::keys::{generate_dek, generate_nonce_prefix, make_aead_key, MasterKey};
use crate::encryption::stream::{DecryptingStream, EncryptingStream, PlaintextStats};

/// Algorithm identifier persisted in the sidecar/row for SSE-S3 (and SSE-KMS).
pub const AES256: &str = "AES256";
/// Algorithm identifier for SSE-C (customer-managed key) — never re-encryptable.
pub const SSEC: &str = "SSE-C";

/// Direction of a re-encryption job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecryptDirection {
    /// Plaintext blobs → SSE-S3 (AES256).
    Encrypt,
    /// SSE-S3 (AES256) blobs → plaintext.
    Decrypt,
}

/// Why a blob is skipped by a re-encryption job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// SSE-C object: the customer holds the key, the server cannot re-encrypt it.
    CustomerKey,
    /// Already in the target state for this direction (nothing to do).
    AlreadyTarget,
    /// Composite (multipart) blob: its parts are re-encrypted individually and
    /// the parent composite has no physical file of its own (TD-014).
    Composite,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::CustomerKey => "customer-key",
            Self::AlreadyTarget => "already-target",
            Self::Composite => "composite",
        }
    }
}

/// Outcome of an encrypt transform, returned so the caller can persist the
/// matching sidecar + object-row metadata and sanity-check the result.
#[derive(Debug, Clone)]
pub struct EncryptOutcome {
    /// Plaintext size in bytes (unchanged from the original object).
    pub plaintext_size: u64,
    /// Hex-encoded MD5 of the plaintext (= the object's S3 ETag). Must equal
    /// the pre-existing ETag — re-encryption never changes the logical object.
    pub plaintext_etag: String,
    /// Encryption metadata to write into the sidecar and the object row.
    pub encryption: BlobEncryptionInfo,
}

/// Decides whether a sidecar's blob must be skipped for the given direction.
///
/// `None` means "process this blob"; `Some(reason)` means skip it. The order of
/// checks is deliberate: composite and SSE-C are hard skips regardless of
/// direction; only then do we consider whether the blob is already in the
/// target state.
pub fn classify_skip(meta: &SidecarMeta, direction: RecryptDirection) -> Option<SkipReason> {
    if meta.composite.is_some() {
        return Some(SkipReason::Composite);
    }
    if let Some(enc) = &meta.encryption {
        if enc.algorithm == SSEC {
            return Some(SkipReason::CustomerKey);
        }
    }
    let is_aes256 = meta
        .encryption
        .as_ref()
        .is_some_and(|e| e.algorithm == AES256);
    match direction {
        RecryptDirection::Encrypt if is_aes256 => Some(SkipReason::AlreadyTarget),
        RecryptDirection::Decrypt if meta.encryption.is_none() => Some(SkipReason::AlreadyTarget),
        _ => None,
    }
}

/// Builds an encrypting stream over `plain`, returning the stream (to be driven
/// into a writer), the shared plaintext stats (size + MD5, populated only after
/// the stream is fully consumed), and the [`BlobEncryptionInfo`] (known up
/// front). Mirrors `EncryptingBlobStore::put` without involving a `BlobStore`.
pub fn build_encryptor(
    plain: ByteStream,
    master_key: &MasterKey,
) -> Result<(ByteStream, Arc<Mutex<PlaintextStats>>, BlobEncryptionInfo), ArcaError> {
    let b64 = &base64::engine::general_purpose::STANDARD;

    let dek = generate_dek().map_err(|e| ArcaError::Internal(format!("generate DEK: {e}")))?;
    let nonce_prefix =
        generate_nonce_prefix().map_err(|e| ArcaError::Internal(format!("generate nonce: {e}")))?;
    let (encrypted_dek, dek_nonce) = master_key
        .wrap_dek(&dek)
        .map_err(|e| ArcaError::Internal(format!("wrap DEK: {e}")))?;
    let key = make_aead_key(&dek).map_err(|e| ArcaError::Internal(format!("AEAD key: {e}")))?;

    let (enc_stream, stats) =
        EncryptingStream::new(plain, key, nonce_prefix, format::DEFAULT_CHUNK_SIZE);

    let info = BlobEncryptionInfo {
        algorithm: AES256.to_string(),
        encrypted_dek: b64.encode(&encrypted_dek),
        dek_nonce: b64.encode(&dek_nonce),
        nonce_prefix: b64.encode(nonce_prefix),
        key_id: master_key.key_id().to_string(),
    };

    Ok((Box::pin(enc_stream), stats, info))
}

/// Builds a decrypting stream over ciphertext that FOLLOWS the 9-byte file
/// header (the caller must strip/skip the header first). Unwraps the per-object
/// DEK from `enc_info` with the master key. Mirrors the
/// `EncryptingBlobStore::get` decrypt path.
pub fn build_decryptor(
    cipher_after_header: ByteStream,
    enc_info: &BlobEncryptionInfo,
    master_key: &MasterKey,
) -> Result<ByteStream, ArcaError> {
    let b64 = &base64::engine::general_purpose::STANDARD;

    let encrypted_dek = b64
        .decode(&enc_info.encrypted_dek)
        .map_err(|e| ArcaError::Internal(format!("decode encrypted_dek: {e}")))?;
    let dek_nonce = b64
        .decode(&enc_info.dek_nonce)
        .map_err(|e| ArcaError::Internal(format!("decode dek_nonce: {e}")))?;
    let nonce_prefix_bytes = b64
        .decode(&enc_info.nonce_prefix)
        .map_err(|e| ArcaError::Internal(format!("decode nonce_prefix: {e}")))?;
    if nonce_prefix_bytes.len() != 4 {
        return Err(ArcaError::Internal("nonce_prefix must be 4 bytes".into()));
    }
    let mut nonce_prefix = [0u8; 4];
    nonce_prefix.copy_from_slice(&nonce_prefix_bytes);

    let dek = master_key
        .unwrap_dek(&encrypted_dek, &dek_nonce)
        .map_err(|e| ArcaError::DecryptionFailed(format!("unwrap DEK: {e}")))?;
    let key = make_aead_key(&dek).map_err(|e| ArcaError::Internal(format!("AEAD key: {e}")))?;

    Ok(Box::pin(DecryptingStream::new(
        cipher_after_header,
        key,
        nonce_prefix,
    )))
}

/// Reads the first [`format::HEADER_SIZE`] bytes of `path` and reports whether
/// it is an `AENC`-framed encrypted blob. Used for idempotency: re-running an
/// encrypt job must not double-encrypt, and a decrypt job must skip plaintext.
async fn is_encrypted_file(path: &Path) -> Result<bool, ArcaError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ArcaError::Internal(format!("open {}: {e}", path.display())))?;
    let mut head = [0u8; format::HEADER_SIZE];
    let n = file
        .read(&mut head)
        .await
        .map_err(|e| ArcaError::Internal(format!("read header {}: {e}", path.display())))?;
    Ok(n >= 4 && &head[0..4] == format::MAGIC)
}

/// Drives `stream` into a fresh temp file next to `path`, then atomically
/// renames it over `path`. The temp uses `extension` so concurrent jobs on
/// distinct blobs never collide.
async fn stream_to_file_atomic(
    mut stream: ByteStream,
    path: &Path,
    extension: &str,
) -> Result<(), ArcaError> {
    let tmp = path.with_extension(extension);
    let file = tokio::fs::File::create(&tmp)
        .await
        .map_err(|e| ArcaError::Internal(format!("create {}: {e}", tmp.display())))?;
    let mut writer = tokio::io::BufWriter::new(file);

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| ArcaError::Internal(format!("re-crypt stream: {e}")))?;
        writer
            .write_all(&chunk)
            .await
            .map_err(|e| ArcaError::Internal(format!("write {}: {e}", tmp.display())))?;
    }
    writer
        .flush()
        .await
        .map_err(|e| ArcaError::Internal(format!("flush {}: {e}", tmp.display())))?;
    writer
        .into_inner()
        .sync_all()
        .await
        .map_err(|e| ArcaError::Internal(format!("fsync {}: {e}", tmp.display())))?;

    tokio::fs::rename(&tmp, path)
        .await
        .map_err(|e| ArcaError::Internal(format!("rename {} -> {}: {e}", tmp.display(), path.display())))?;
    Ok(())
}

/// Encrypts a plaintext blob file in place (atomic temp + rename).
///
/// Idempotent: returns `Ok(None)` if the file is already an `AENC`-encrypted
/// blob (so re-running a job never double-encrypts). On success returns the
/// [`EncryptOutcome`] so the caller can persist the sidecar and the object row
/// (`encryption_algorithm`, `encryption_key_id`). The plaintext ETag in the
/// outcome MUST match the object's existing ETag — re-encryption preserves the
/// logical object byte-for-byte.
pub async fn encrypt_file_in_place(
    path: &Path,
    master_key: &MasterKey,
) -> Result<Option<EncryptOutcome>, ArcaError> {
    if is_encrypted_file(path).await? {
        return Ok(None);
    }

    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ArcaError::Internal(format!("open {}: {e}", path.display())))?;
    let plain: ByteStream = Box::pin(ReaderStream::with_capacity(file, 65536));

    let (enc_stream, stats, info) = build_encryptor(plain, master_key)?;
    stream_to_file_atomic(enc_stream, path, "aenc-tmp").await?;

    let stats = stats.lock().unwrap();
    let md5 = stats
        .md5
        .ok_or_else(|| ArcaError::Internal("plaintext MD5 not computed".into()))?;
    Ok(Some(EncryptOutcome {
        plaintext_size: stats.size,
        plaintext_etag: hex::encode(md5),
        encryption: info,
    }))
}

/// Decrypts an `AES256` blob file back to plaintext in place (atomic).
///
/// Idempotent: returns `Ok(false)` if the file is not an `AENC` blob (already
/// plaintext). `enc_info` is the blob's encryption metadata (from its sidecar).
/// On success the file holds the original plaintext and the caller should clear
/// `encryption` from the sidecar and the object row.
pub async fn decrypt_file_in_place(
    path: &Path,
    enc_info: &BlobEncryptionInfo,
    master_key: &MasterKey,
) -> Result<bool, ArcaError> {
    if !is_encrypted_file(path).await? {
        return Ok(false);
    }

    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| ArcaError::Internal(format!("open {}: {e}", path.display())))?;
    // Validate + skip the 9-byte header before streaming the ciphertext.
    let mut head = [0u8; format::HEADER_SIZE];
    file.read_exact(&mut head)
        .await
        .map_err(|e| ArcaError::Internal(format!("read header {}: {e}", path.display())))?;
    format::parse_header(&head).map_err(|e| ArcaError::Internal(format!("parse header: {e}")))?;
    file.seek(std::io::SeekFrom::Start(format::HEADER_SIZE as u64))
        .await
        .map_err(|e| ArcaError::Internal(format!("seek {}: {e}", path.display())))?;

    let cipher: ByteStream = Box::pin(ReaderStream::with_capacity(file, 65536));
    let plain = build_decryptor(cipher, enc_info, master_key)?;
    stream_to_file_atomic(plain, path, "plain-tmp").await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio_stream::iter as stream_iter;

    fn master_key() -> MasterKey {
        MasterKey::from_bytes(&[0x7Au8; 32]).unwrap()
    }

    fn bytes_to_stream(data: &[u8]) -> ByteStream {
        Box::pin(stream_iter(vec![Ok(Bytes::copy_from_slice(data))]))
    }

    async fn collect(mut s: ByteStream) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(c) = s.next().await {
            out.extend_from_slice(&c.unwrap());
        }
        out
    }

    fn sidecar_with(encryption: Option<BlobEncryptionInfo>) -> SidecarMeta {
        SidecarMeta {
            bucket: "b".into(),
            key: "k".into(),
            size: 10,
            etag: "e".into(),
            content_type: None,
            last_modified: "2026-06-24T00:00:00Z".into(),
            metadata: Default::default(),
            encryption,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    fn aes_info() -> BlobEncryptionInfo {
        BlobEncryptionInfo {
            algorithm: AES256.into(),
            encrypted_dek: "x".into(),
            dek_nonce: "x".into(),
            nonce_prefix: "x".into(),
            key_id: "deadbeef".into(),
        }
    }

    fn ssec_info() -> BlobEncryptionInfo {
        BlobEncryptionInfo {
            algorithm: SSEC.into(),
            encrypted_dek: String::new(),
            dek_nonce: String::new(),
            nonce_prefix: "AAAAAA==".into(),
            key_id: String::new(),
        }
    }

    #[test]
    fn classify_encrypt_plaintext_is_processed() {
        assert_eq!(
            classify_skip(&sidecar_with(None), RecryptDirection::Encrypt),
            None
        );
    }

    #[test]
    fn classify_encrypt_already_aes256_skips() {
        assert_eq!(
            classify_skip(&sidecar_with(Some(aes_info())), RecryptDirection::Encrypt),
            Some(SkipReason::AlreadyTarget)
        );
    }

    #[test]
    fn classify_decrypt_plaintext_skips() {
        assert_eq!(
            classify_skip(&sidecar_with(None), RecryptDirection::Decrypt),
            Some(SkipReason::AlreadyTarget)
        );
    }

    #[test]
    fn classify_decrypt_aes256_is_processed() {
        assert_eq!(
            classify_skip(&sidecar_with(Some(aes_info())), RecryptDirection::Decrypt),
            None
        );
    }

    #[test]
    fn classify_ssec_always_skips() {
        assert_eq!(
            classify_skip(&sidecar_with(Some(ssec_info())), RecryptDirection::Encrypt),
            Some(SkipReason::CustomerKey)
        );
        assert_eq!(
            classify_skip(&sidecar_with(Some(ssec_info())), RecryptDirection::Decrypt),
            Some(SkipReason::CustomerKey)
        );
    }

    #[test]
    fn classify_composite_always_skips() {
        let mut meta = sidecar_with(None);
        meta.composite = Some(vec![]);
        assert_eq!(
            classify_skip(&meta, RecryptDirection::Encrypt),
            Some(SkipReason::Composite)
        );
    }

    #[tokio::test]
    async fn stream_encrypt_decrypt_roundtrip() {
        let mk = master_key();
        let plaintext = b"the quick brown fox jumps over the lazy dog".repeat(1000);

        let (enc_stream, stats, info) = build_encryptor(bytes_to_stream(&plaintext), &mk).unwrap();
        let encrypted = collect(enc_stream).await;

        // Framed exactly like a live encrypted blob.
        assert_eq!(&encrypted[..4], format::MAGIC);
        assert!(encrypted.len() > format::HEADER_SIZE);
        assert_eq!(info.algorithm, AES256);
        assert_eq!(info.key_id, mk.key_id());

        let stats = stats.lock().unwrap();
        assert_eq!(stats.size, plaintext.len() as u64);
        drop(stats);

        // Strip the header, decrypt, and confirm we get the plaintext back.
        let cipher = bytes_to_stream(&encrypted[format::HEADER_SIZE..]);
        let dec_stream = build_decryptor(cipher, &info, &mk).unwrap();
        let decrypted = collect(dec_stream).await;
        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn file_roundtrip_and_idempotency() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        let plaintext = b"in-place re-encryption payload \x00\x01\x02".repeat(5000);
        tokio::fs::write(&path, &plaintext).await.unwrap();

        let mk = master_key();

        // Encrypt in place.
        let outcome = encrypt_file_in_place(&path, &mk).await.unwrap().unwrap();
        assert_eq!(outcome.plaintext_size, plaintext.len() as u64);
        let on_disk = tokio::fs::read(&path).await.unwrap();
        assert_eq!(&on_disk[..4], format::MAGIC);
        assert_ne!(on_disk, plaintext);

        // The plaintext ETag matches a plain MD5 of the original bytes.
        let expected_etag = {
            use md5::{Digest, Md5};
            let mut h = Md5::new();
            h.update(&plaintext);
            hex::encode::<[u8; 16]>(h.finalize().into())
        };
        assert_eq!(outcome.plaintext_etag, expected_etag);

        // Encrypting again is a no-op (already AENC).
        assert!(encrypt_file_in_place(&path, &mk).await.unwrap().is_none());

        // Decrypt in place restores the exact original bytes.
        assert!(decrypt_file_in_place(&path, &outcome.encryption, &mk)
            .await
            .unwrap());
        let restored = tokio::fs::read(&path).await.unwrap();
        assert_eq!(restored, plaintext);

        // Decrypting again is a no-op (already plaintext).
        assert!(!decrypt_file_in_place(&path, &outcome.encryption, &mk)
            .await
            .unwrap());
    }
}
