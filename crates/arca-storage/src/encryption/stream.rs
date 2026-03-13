//! Encrypting and decrypting byte streams for chunk-based AES-256-GCM.
//!
//! `EncryptingStream` wraps a plaintext `ByteStream`, buffers into fixed-size
//! chunks, encrypts each chunk with incrementing nonces, and yields the
//! encrypted output including the file header. It simultaneously computes
//! MD5 over the original plaintext for S3 ETag generation.
//!
//! `DecryptingStream` wraps an encrypted byte stream (after the header),
//! parses each chunk, decrypts it, and yields the plaintext.
//!
//! For byte range reads on encrypted blobs, use `decrypt_range()` which
//! decrypts only the chunks overlapping the requested range.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use md5::{Digest, Md5};
use ring::aead::LessSafeKey;

use super::format::{self, TAG_LEN};
use super::keys::{build_nonce, decrypt_chunk, encrypt_chunk};

/// Plaintext statistics captured during encryption.
#[derive(Debug, Default)]
pub struct PlaintextStats {
    /// Total plaintext size in bytes.
    pub size: u64,
    /// MD5 digest of plaintext (set after stream is fully consumed).
    pub md5: Option<[u8; 16]>,
}

/// Encrypting stream that wraps a plaintext `ByteStream`.
///
/// Buffers incoming plaintext into fixed-size chunks, encrypts each chunk
/// with AES-256-GCM using incrementing nonces, and yields the encrypted
/// output. The first yield is always the 9-byte file header.
///
/// Simultaneously computes MD5 over the original plaintext. After the
/// stream is fully consumed, read the shared `PlaintextStats` to get
/// the plaintext size and MD5 hash.
pub struct EncryptingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
    key: LessSafeKey,
    nonce_prefix: [u8; 4],
    chunk_size: u32,
    chunk_index: u64,
    buffer: Vec<u8>,
    hasher: Md5,
    plaintext_size: u64,
    header_emitted: bool,
    inner_done: bool,
    stats: Arc<Mutex<PlaintextStats>>,
    /// Encrypted chunks ready to yield (usually 0 or 1).
    pending: Vec<Bytes>,
}

impl EncryptingStream {
    /// Creates a new encrypting stream.
    ///
    /// Returns the stream and a shared handle to read plaintext stats
    /// after the stream is fully consumed.
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        key: LessSafeKey,
        nonce_prefix: [u8; 4],
        chunk_size: u32,
    ) -> (Self, Arc<Mutex<PlaintextStats>>) {
        let stats = Arc::new(Mutex::new(PlaintextStats::default()));
        let stream = Self {
            inner,
            key,
            nonce_prefix,
            chunk_size,
            chunk_index: 0,
            buffer: Vec::with_capacity(chunk_size as usize),
            hasher: Md5::new(),
            plaintext_size: 0,
            header_emitted: false,
            inner_done: false,
            stats: stats.clone(),
            pending: Vec::new(),
        };
        (stream, stats)
    }

    /// Encrypts the current buffer as one chunk and pushes to `pending`.
    fn flush_chunk(&mut self) -> Result<(), io::Error> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let plaintext_len = self.buffer.len() as u32;
        let nonce = build_nonce(&self.nonce_prefix, self.chunk_index);
        let ciphertext = encrypt_chunk(&self.key, &nonce, &self.buffer)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let mut chunk_bytes = Vec::with_capacity(4 + ciphertext.len());
        chunk_bytes.extend_from_slice(&plaintext_len.to_le_bytes());
        chunk_bytes.extend_from_slice(&ciphertext);

        self.pending.push(Bytes::from(chunk_bytes));
        self.chunk_index += 1;
        self.buffer.clear();
        Ok(())
    }

    /// Flushes the remaining buffer and writes final stats.
    fn finalize(&mut self) -> Result<(), io::Error> {
        self.flush_chunk()?;
        let digest = self.hasher.clone().finalize();
        let mut stats = self.stats.lock().unwrap();
        stats.size = self.plaintext_size;
        stats.md5 = Some(digest.into());
        Ok(())
    }
}

impl Stream for EncryptingStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // 1. Emit the file header first.
        if !this.header_emitted {
            this.header_emitted = true;
            let header = format::write_header(this.chunk_size);
            return Poll::Ready(Some(Ok(Bytes::from(header))));
        }

        // 2. Yield any pending encrypted chunks from previous polls.
        if !this.pending.is_empty() {
            return Poll::Ready(Some(Ok(this.pending.remove(0))));
        }

        // 3. Stream is done.
        if this.inner_done {
            return Poll::Ready(None);
        }

        // 4. Poll the inner stream until we produce output or get Pending/None.
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.hasher.update(&chunk);
                    this.plaintext_size += chunk.len() as u64;

                    let mut remaining = chunk.as_ref();
                    while !remaining.is_empty() {
                        let space = this.chunk_size as usize - this.buffer.len();
                        let take = remaining.len().min(space);
                        this.buffer.extend_from_slice(&remaining[..take]);
                        remaining = &remaining[take..];

                        if this.buffer.len() >= this.chunk_size as usize {
                            if let Err(e) = this.flush_chunk() {
                                return Poll::Ready(Some(Err(e)));
                            }
                        }
                    }

                    if !this.pending.is_empty() {
                        return Poll::Ready(Some(Ok(this.pending.remove(0))));
                    }
                    // Buffer not yet full — continue polling inner.
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    this.inner_done = true;
                    if let Err(e) = this.finalize() {
                        return Poll::Ready(Some(Err(e)));
                    }
                    if !this.pending.is_empty() {
                        return Poll::Ready(Some(Ok(this.pending.remove(0))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Decrypting stream that wraps an encrypted byte stream (after the header).
///
/// Parses each on-disk chunk (4-byte LE plaintext_len + ciphertext + tag),
/// decrypts it, and yields the plaintext.
pub struct DecryptingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
    key: LessSafeKey,
    nonce_prefix: [u8; 4],
    chunk_index: u64,
    /// Accumulation buffer for encrypted bytes.
    buffer: Vec<u8>,
    inner_done: bool,
}

impl DecryptingStream {
    /// Creates a new decrypting stream.
    ///
    /// `inner` must yield encrypted bytes *after* the 9-byte file header
    /// (the header should be parsed/skipped before creating this stream).
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        key: LessSafeKey,
        nonce_prefix: [u8; 4],
    ) -> Self {
        Self {
            inner,
            key,
            nonce_prefix,
            chunk_index: 0,
            buffer: Vec::new(),
            inner_done: false,
        }
    }

    /// Tries to extract and decrypt one chunk from the buffer.
    /// Returns `Some(plaintext)` if a complete chunk was available.
    fn try_decrypt_chunk(&mut self) -> Result<Option<Bytes>, io::Error> {
        // Need at least 4 bytes for the plaintext length prefix.
        if self.buffer.len() < 4 {
            return Ok(None);
        }

        let plaintext_len =
            u32::from_le_bytes([self.buffer[0], self.buffer[1], self.buffer[2], self.buffer[3]])
                as usize;
        let on_disk_chunk_len = 4 + plaintext_len + TAG_LEN;

        if self.buffer.len() < on_disk_chunk_len {
            return Ok(None);
        }

        let ciphertext_and_tag = &self.buffer[4..on_disk_chunk_len];
        let nonce = build_nonce(&self.nonce_prefix, self.chunk_index);
        let plaintext = decrypt_chunk(&self.key, &nonce, ciphertext_and_tag)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        // Remove the consumed chunk from the buffer.
        self.buffer.drain(..on_disk_chunk_len);
        self.chunk_index += 1;

        Ok(Some(Bytes::from(plaintext)))
    }
}

impl Stream for DecryptingStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Try to decrypt from what we already have.
        match this.try_decrypt_chunk() {
            Ok(Some(plaintext)) => return Poll::Ready(Some(Ok(plaintext))),
            Ok(None) => {}
            Err(e) => return Poll::Ready(Some(Err(e))),
        }

        if this.inner_done {
            // No more data and nothing left to decrypt.
            if this.buffer.is_empty() {
                return Poll::Ready(None);
            }
            // Leftover bytes that don't form a complete chunk.
            return Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated encrypted chunk",
            ))));
        }

        // Poll inner for more encrypted bytes.
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);

                    match this.try_decrypt_chunk() {
                        Ok(Some(plaintext)) => return Poll::Ready(Some(Ok(plaintext))),
                        Ok(None) => {
                            // Need more data, continue polling.
                        }
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    this.inner_done = true;
                    // Try one last decrypt with whatever we have.
                    match this.try_decrypt_chunk() {
                        Ok(Some(plaintext)) => return Poll::Ready(Some(Ok(plaintext))),
                        Ok(None) => {
                            if this.buffer.is_empty() {
                                return Poll::Ready(None);
                            }
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "truncated encrypted chunk",
                            ))));
                        }
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Byte range helpers
// ---------------------------------------------------------------------------

/// Calculates the on-disk byte offset of chunk `i` (0-based).
///
/// Accounts for the 9-byte file header and fixed on-disk chunk sizes.
pub fn chunk_disk_offset(chunk_index: u64, chunk_size: u32) -> u64 {
    format::HEADER_SIZE as u64 + chunk_index * format::on_disk_chunk_size(chunk_size)
}

/// Decrypts a contiguous slice of encrypted chunks and returns the
/// plaintext corresponding to the requested byte range.
///
/// `encrypted_data` should contain the raw on-disk bytes for the
/// relevant chunks (starting at the first overlapping chunk).
/// `first_chunk_index` is the 0-based index of the first chunk.
/// `range_start` and `range_end` are the inclusive plaintext byte offsets.
pub fn decrypt_range(
    key: &LessSafeKey,
    nonce_prefix: &[u8; 4],
    chunk_size: u32,
    encrypted_data: &[u8],
    first_chunk_index: u64,
    range_start: u64,
    range_end: u64,
) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    let mut offset = 0usize;
    let mut chunk_idx = first_chunk_index;

    while offset < encrypted_data.len() {
        if offset + 4 > encrypted_data.len() {
            return Err("truncated chunk length prefix".to_string());
        }
        let plaintext_len = u32::from_le_bytes([
            encrypted_data[offset],
            encrypted_data[offset + 1],
            encrypted_data[offset + 2],
            encrypted_data[offset + 3],
        ]) as usize;

        let on_disk_len = 4 + plaintext_len + TAG_LEN;
        if offset + on_disk_len > encrypted_data.len() {
            return Err("truncated encrypted chunk".to_string());
        }

        let ciphertext_and_tag = &encrypted_data[offset + 4..offset + on_disk_len];

        // This chunk covers plaintext bytes [chunk_start, chunk_start + plaintext_len).
        let chunk_start = chunk_idx * chunk_size as u64;
        let chunk_pt_end = chunk_start + plaintext_len as u64; // exclusive

        // Skip chunks entirely outside the requested range.
        if chunk_pt_end <= range_start || chunk_start > range_end {
            offset += on_disk_len;
            chunk_idx += 1;
            continue;
        }

        let nonce = build_nonce(nonce_prefix, chunk_idx);
        let plaintext = decrypt_chunk(key, &nonce, ciphertext_and_tag)?;

        // Intersect with the requested range.
        let slice_start = if range_start > chunk_start {
            (range_start - chunk_start) as usize
        } else {
            0
        };
        let slice_end = if range_end + 1 < chunk_pt_end {
            (range_end + 1 - chunk_start) as usize
        } else {
            plaintext.len()
        };

        if slice_start < slice_end {
            result.extend_from_slice(&plaintext[slice_start..slice_end]);
        }

        offset += on_disk_len;
        chunk_idx += 1;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encryption::format::{self, DEFAULT_CHUNK_SIZE};
    use crate::encryption::keys::{generate_dek, generate_nonce_prefix, make_aead_key};
    use bytes::Bytes;
    use tokio_stream::iter as stream_iter;
    use tokio_stream::StreamExt;

    type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

    fn bytes_to_stream(data: &[u8]) -> ByteStream {
        let chunks = vec![Ok(Bytes::copy_from_slice(data))];
        Box::pin(stream_iter(chunks))
    }

    fn multi_chunk_stream(chunks: Vec<&[u8]>) -> ByteStream {
        let items: Vec<Result<Bytes, io::Error>> = chunks
            .into_iter()
            .map(|c| Ok(Bytes::copy_from_slice(c)))
            .collect();
        Box::pin(stream_iter(items))
    }

    async fn collect_stream(stream: ByteStream) -> Vec<u8> {
        let mut stream = std::pin::pin!(stream);
        let mut buf = Vec::new();
        while let Some(chunk) = stream.as_mut().next().await {
            buf.extend_from_slice(&chunk.unwrap());
        }
        buf
    }

    /// Returns raw DEK bytes, nonce prefix, and a fresh LessSafeKey.
    /// Keep the raw bytes so you can create a second key for decrypt.
    fn test_dek_and_prefix() -> ([u8; 32], [u8; 4]) {
        let dek = generate_dek().unwrap();
        let prefix = generate_nonce_prefix().unwrap();
        (dek, prefix)
    }

    // ----- EncryptingStream tests -----

    #[tokio::test]
    async fn encrypt_small_data() {
        let (dek, prefix) = test_dek_and_prefix();
        let key = make_aead_key(&dek).unwrap();
        let plaintext = b"hello, encryption!";
        let stream = bytes_to_stream(plaintext);

        let (enc_stream, stats) = EncryptingStream::new(stream, key, prefix, DEFAULT_CHUNK_SIZE);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Should start with header.
        assert_eq!(&encrypted[..4], format::MAGIC);
        assert_eq!(encrypted[4], format::VERSION);
        assert!(encrypted.len() > format::HEADER_SIZE);

        // Stats should have correct plaintext size and MD5.
        let stats = stats.lock().unwrap();
        assert_eq!(stats.size, plaintext.len() as u64);
        assert!(stats.md5.is_some());

        let expected_md5 = {
            let mut h = Md5::new();
            h.update(plaintext);
            let d: [u8; 16] = h.finalize().into();
            d
        };
        assert_eq!(stats.md5.unwrap(), expected_md5);
    }

    #[tokio::test]
    async fn encrypt_empty_data() {
        let (dek, prefix) = test_dek_and_prefix();
        let key = make_aead_key(&dek).unwrap();
        let stream = bytes_to_stream(b"");

        let (enc_stream, stats) = EncryptingStream::new(stream, key, prefix, DEFAULT_CHUNK_SIZE);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Only the header — no chunks (empty buffer is not flushed).
        assert_eq!(encrypted.len(), format::HEADER_SIZE);

        let stats = stats.lock().unwrap();
        assert_eq!(stats.size, 0);
    }

    #[tokio::test]
    async fn encrypt_exact_chunk_boundary() {
        let (dek, prefix) = test_dek_and_prefix();
        let key = make_aead_key(&dek).unwrap();
        let chunk_size = 16u32;
        let plaintext = vec![0xABu8; 16]; // Exactly one chunk.
        let stream = bytes_to_stream(&plaintext);

        let (enc_stream, stats) = EncryptingStream::new(stream, key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        let stats = stats.lock().unwrap();
        assert_eq!(stats.size, 16);

        // Header (9) + 1 chunk (4 len + 16 ciphertext + 16 tag = 36) = 45.
        assert_eq!(encrypted.len(), 9 + 4 + 16 + TAG_LEN);
    }

    #[tokio::test]
    async fn encrypt_multiple_chunks() {
        let (dek, prefix) = test_dek_and_prefix();
        let key = make_aead_key(&dek).unwrap();
        let chunk_size = 16u32;
        let plaintext = vec![0xCDu8; 50]; // 16 + 16 + 16 + 2 = 4 chunks.
        let stream = bytes_to_stream(&plaintext);

        let (enc_stream, stats) = EncryptingStream::new(stream, key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        let stats = stats.lock().unwrap();
        assert_eq!(stats.size, 50);

        // Header (9) + 3 full chunks (4+16+16=36 each) + 1 partial (4+2+16=22) = 9 + 108 + 22 = 139.
        assert_eq!(encrypted.len(), 9 + 3 * 36 + (4 + 2 + TAG_LEN));
    }

    #[tokio::test]
    async fn encrypt_fragmented_input() {
        let (dek, prefix) = test_dek_and_prefix();
        let key = make_aead_key(&dek).unwrap();
        let chunk_size = 16u32;
        let stream = multi_chunk_stream(vec![b"hello", b" ", b"world", b"!padding1234"]);

        let (enc_stream, stats) = EncryptingStream::new(stream, key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        let stats = stats.lock().unwrap();
        // "hello" + " " + "world" + "!padding1234" = 5+1+5+12 = 23
        assert_eq!(stats.size, 23);

        // 1 full chunk (16 bytes) + 1 partial (7 bytes).
        let expected = 9 + (4 + 16 + TAG_LEN) + (4 + 7 + TAG_LEN);
        assert_eq!(encrypted.len(), expected);
    }

    // ----- DecryptingStream tests -----

    #[tokio::test]
    async fn encrypt_decrypt_roundtrip() {
        let (dek, prefix) = test_dek_and_prefix();
        let plaintext = b"roundtrip test data for encryption";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) =
            EncryptingStream::new(stream, enc_key, prefix, DEFAULT_CHUNK_SIZE);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        let _chunk_size = format::parse_header(&encrypted).unwrap();
        let cipher_stream = bytes_to_stream(&encrypted[format::HEADER_SIZE..]);
        let dec_key = make_aead_key(&dek).unwrap();
        let dec_stream = DecryptingStream::new(cipher_stream, dec_key, prefix);
        let decrypted = collect_stream(Box::pin(dec_stream)).await;

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn encrypt_decrypt_empty() {
        let (dek, prefix) = test_dek_and_prefix();
        let stream = bytes_to_stream(b"");

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) =
            EncryptingStream::new(stream, enc_key, prefix, DEFAULT_CHUNK_SIZE);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        let cipher_stream = bytes_to_stream(&encrypted[format::HEADER_SIZE..]);
        let dec_key = make_aead_key(&dek).unwrap();
        let dec_stream = DecryptingStream::new(cipher_stream, dec_key, prefix);
        let decrypted = collect_stream(Box::pin(dec_stream)).await;

        assert!(decrypted.is_empty());
    }

    #[tokio::test]
    async fn encrypt_decrypt_multiple_chunks() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 16u32;
        let plaintext = vec![0xEFu8; 50];
        let stream = bytes_to_stream(&plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        let parsed_chunk_size = format::parse_header(&encrypted).unwrap();
        assert_eq!(parsed_chunk_size, chunk_size);

        let cipher_stream = bytes_to_stream(&encrypted[format::HEADER_SIZE..]);
        let dec_key = make_aead_key(&dek).unwrap();
        let dec_stream = DecryptingStream::new(cipher_stream, dec_key, prefix);
        let decrypted = collect_stream(Box::pin(dec_stream)).await;

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn encrypt_decrypt_large_data() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 64u32;
        let plaintext: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let stream = bytes_to_stream(&plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        let cipher_stream = bytes_to_stream(&encrypted[format::HEADER_SIZE..]);
        let dec_key = make_aead_key(&dek).unwrap();
        let dec_stream = DecryptingStream::new(cipher_stream, dec_key, prefix);
        let decrypted = collect_stream(Box::pin(dec_stream)).await;

        assert_eq!(decrypted, plaintext);
    }

    #[tokio::test]
    async fn decrypt_corrupted_data_fails() {
        let (dek, prefix) = test_dek_and_prefix();
        let plaintext = b"corruption test";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) =
            EncryptingStream::new(stream, enc_key, prefix, DEFAULT_CHUNK_SIZE);
        let mut encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Corrupt a ciphertext byte (after header + 4-byte length prefix).
        if encrypted.len() > format::HEADER_SIZE + 5 {
            encrypted[format::HEADER_SIZE + 5] ^= 0xFF;
        }

        let cipher_stream = bytes_to_stream(&encrypted[format::HEADER_SIZE..]);
        let dec_key = make_aead_key(&dek).unwrap();
        let dec_stream = DecryptingStream::new(cipher_stream, dec_key, prefix);
        let mut stream = std::pin::pin!(dec_stream);
        let mut had_error = false;
        while let Some(result) = stream.as_mut().next().await {
            if result.is_err() {
                had_error = true;
                break;
            }
        }
        assert!(had_error);
    }

    #[tokio::test]
    async fn decrypt_truncated_data_fails() {
        let (dek, prefix) = test_dek_and_prefix();
        let plaintext = b"truncation test";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) =
            EncryptingStream::new(stream, enc_key, prefix, DEFAULT_CHUNK_SIZE);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Truncate the encrypted data (remove last 5 bytes).
        let truncated = &encrypted[format::HEADER_SIZE..encrypted.len() - 5];
        let cipher_stream = bytes_to_stream(truncated);
        let dec_key = make_aead_key(&dek).unwrap();
        let dec_stream = DecryptingStream::new(cipher_stream, dec_key, prefix);
        let mut stream = std::pin::pin!(dec_stream);
        let mut had_error = false;
        while let Some(result) = stream.as_mut().next().await {
            if result.is_err() {
                had_error = true;
                break;
            }
        }
        assert!(had_error);
    }

    #[tokio::test]
    async fn decrypt_fragmented_encrypted_input() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 16u32;
        let plaintext = b"fragmented input decryption test!!";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Feed encrypted data in small fragments (7 bytes at a time).
        let cipher_data = &encrypted[format::HEADER_SIZE..];
        let fragments: Vec<&[u8]> = cipher_data.chunks(7).collect();
        let cipher_stream = multi_chunk_stream(fragments);
        let dec_key = make_aead_key(&dek).unwrap();
        let dec_stream = DecryptingStream::new(cipher_stream, dec_key, prefix);
        let decrypted = collect_stream(Box::pin(dec_stream)).await;

        assert_eq!(decrypted, plaintext);
    }

    // ----- Range decryption tests -----

    #[tokio::test]
    async fn range_read_single_chunk() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 16u32;
        let plaintext = b"0123456789abcdef"; // Exactly 1 chunk.
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Range: bytes 3..=7 (inclusive) -> "34567"
        let chunk_data = &encrypted[format::HEADER_SIZE..];
        let dec_key = make_aead_key(&dek).unwrap();
        let result = decrypt_range(&dec_key, &prefix, chunk_size, chunk_data, 0, 3, 7).unwrap();
        assert_eq!(result, b"34567");
    }

    #[tokio::test]
    async fn range_read_cross_chunk_boundary() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 8u32;
        // 24 bytes -> 3 chunks of 8.
        let plaintext = b"AABBCCDDEEEFFGGGHHIIJJKK";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Range: bytes 6..=17 -> spans chunks 0, 1, 2.
        let chunk_data = &encrypted[format::HEADER_SIZE..];
        let dec_key = make_aead_key(&dek).unwrap();
        let result =
            decrypt_range(&dec_key, &prefix, chunk_size, chunk_data, 0, 6, 17).unwrap();
        assert_eq!(result, &plaintext[6..=17]);
    }

    #[tokio::test]
    async fn range_read_start_of_data() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 8u32;
        let plaintext = b"0123456789abcdef";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Range: bytes 0..=4 -> "01234"
        let chunk_data = &encrypted[format::HEADER_SIZE..];
        let dec_key = make_aead_key(&dek).unwrap();
        let result = decrypt_range(&dec_key, &prefix, chunk_size, chunk_data, 0, 0, 4).unwrap();
        assert_eq!(result, b"01234");
    }

    #[tokio::test]
    async fn range_read_end_of_data() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 8u32;
        let plaintext = b"0123456789abcdef";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Range: bytes 12..=15 -> "cdef"
        // Chunk 1 covers bytes 8..15.
        let on_disk_chunk = format::on_disk_chunk_size(chunk_size) as usize;
        let chunk1_offset = on_disk_chunk; // Skip chunk 0.
        let chunk_data = &encrypted[format::HEADER_SIZE + chunk1_offset..];
        let dec_key = make_aead_key(&dek).unwrap();
        let result =
            decrypt_range(&dec_key, &prefix, chunk_size, chunk_data, 1, 12, 15).unwrap();
        assert_eq!(result, b"cdef");
    }

    #[tokio::test]
    async fn range_read_partial_last_chunk() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 8u32;
        let plaintext = b"0123456789ab"; // 12 bytes -> chunks of 8 + 4.
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Range: bytes 6..=11 -> "6789ab" (spans chunk boundary).
        let chunk_data = &encrypted[format::HEADER_SIZE..];
        let dec_key = make_aead_key(&dek).unwrap();
        let result = decrypt_range(&dec_key, &prefix, chunk_size, chunk_data, 0, 6, 11).unwrap();
        assert_eq!(result, b"6789ab");
    }

    #[tokio::test]
    async fn range_read_single_byte() {
        let (dek, prefix) = test_dek_and_prefix();
        let chunk_size = 8u32;
        let plaintext = b"0123456789";
        let stream = bytes_to_stream(plaintext);

        let enc_key = make_aead_key(&dek).unwrap();
        let (enc_stream, _) = EncryptingStream::new(stream, enc_key, prefix, chunk_size);
        let encrypted = collect_stream(Box::pin(enc_stream)).await;

        // Single byte at position 5.
        let chunk_data = &encrypted[format::HEADER_SIZE..];
        let dec_key = make_aead_key(&dek).unwrap();
        let result = decrypt_range(&dec_key, &prefix, chunk_size, chunk_data, 0, 5, 5).unwrap();
        assert_eq!(result, b"5");
    }

    #[tokio::test]
    async fn chunk_disk_offset_calculation() {
        let chunk_size = DEFAULT_CHUNK_SIZE;
        assert_eq!(chunk_disk_offset(0, chunk_size), format::HEADER_SIZE as u64);
        let expected_1 =
            format::HEADER_SIZE as u64 + format::on_disk_chunk_size(chunk_size);
        assert_eq!(chunk_disk_offset(1, chunk_size), expected_1);
        let expected_2 =
            format::HEADER_SIZE as u64 + 2 * format::on_disk_chunk_size(chunk_size);
        assert_eq!(chunk_disk_offset(2, chunk_size), expected_2);
    }
}
