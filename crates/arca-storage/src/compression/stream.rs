//! Compressing and decompressing byte streams with chunk framing.
//!
//! `CompressingStream` wraps a plaintext `ByteStream`, buffers into
//! fixed-size plaintext chunks, compresses each chunk with the configured
//! algorithm, and yields the framed output (header + chunks + footer).
//!
//! `DecompressingStream` decodes framed chunks back to plaintext, either from
//! a bare data region or from a whole stored blob in one pass.
//!
//! Range reads use `decompress_range` against a concrete byte slab already
//! read from disk (analogous to the encryption engine).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use arca_core::store::CompressionAlgorithm;
use bytes::Bytes;
use futures_core::Stream;
use md5::{Digest, Md5};

use super::codec::{compress_chunk, decompress_chunk};
use super::format::{
    self, write_chunk_header, write_footer, write_header, CHUNK_LEN_HEADER, FOOTER_COUNT_SIZE,
    HEADER_SIZE,
};

/// Plaintext statistics captured during compression.
#[derive(Debug, Default)]
pub struct PlaintextStats {
    pub size: u64,
    pub md5: Option<[u8; 16]>,
}

/// Compressing stream that wraps a plaintext `ByteStream`.
///
/// Yields: [header] [chunk0][chunk1]... [footer index].
/// Simultaneously feeds an MD5 hasher over the original plaintext so the
/// ETag stays computed on plaintext.
pub struct CompressingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
    algorithm: CompressionAlgorithm,
    level: i32,
    chunk_size: u32,
    buffer: Vec<u8>,
    hasher: Md5,
    plaintext_size: u64,
    /// On-disk length of each emitted chunk (including per-chunk 8-byte header).
    chunk_on_disk_lens: Vec<u32>,
    pending: Vec<Bytes>,
    header_emitted: bool,
    inner_done: bool,
    footer_emitted: bool,
    stats: Arc<Mutex<PlaintextStats>>,
}

impl CompressingStream {
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        algorithm: CompressionAlgorithm,
        level: i32,
        chunk_size: u32,
    ) -> (Self, Arc<Mutex<PlaintextStats>>) {
        let stats = Arc::new(Mutex::new(PlaintextStats::default()));
        let stream = Self {
            inner,
            algorithm,
            level,
            chunk_size,
            buffer: Vec::with_capacity(chunk_size as usize),
            hasher: Md5::new(),
            plaintext_size: 0,
            chunk_on_disk_lens: Vec::new(),
            pending: Vec::new(),
            header_emitted: false,
            inner_done: false,
            footer_emitted: false,
            stats: stats.clone(),
        };
        (stream, stats)
    }

    /// Compress the current buffer and push the framed chunk into `pending`.
    fn flush_chunk(&mut self) -> Result<(), io::Error> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let plaintext_len = self.buffer.len() as u32;
        let compressed = compress_chunk(self.algorithm, self.level, &self.buffer)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let compressed_len = compressed.len() as u32;

        let mut bytes = Vec::with_capacity(CHUNK_LEN_HEADER + compressed.len());
        bytes.extend_from_slice(&write_chunk_header(compressed_len, plaintext_len));
        bytes.extend_from_slice(&compressed);
        let on_disk = bytes.len() as u32;
        self.chunk_on_disk_lens.push(on_disk);
        self.pending.push(Bytes::from(bytes));
        self.buffer.clear();
        Ok(())
    }

    fn finalize(&mut self) -> Result<(), io::Error> {
        self.flush_chunk()?;
        let digest = self.hasher.clone().finalize();
        let mut stats = self.stats.lock().unwrap();
        stats.size = self.plaintext_size;
        stats.md5 = Some(digest.into());
        Ok(())
    }
}

impl Stream for CompressingStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if !this.header_emitted {
            this.header_emitted = true;
            let header = write_header(this.algorithm, this.chunk_size);
            return Poll::Ready(Some(Ok(Bytes::from(header))));
        }

        if !this.pending.is_empty() {
            return Poll::Ready(Some(Ok(this.pending.remove(0))));
        }

        if this.inner_done {
            if !this.footer_emitted {
                this.footer_emitted = true;
                let footer = write_footer(&this.chunk_on_disk_lens);
                return Poll::Ready(Some(Ok(Bytes::from(footer))));
            }
            return Poll::Ready(None);
        }

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
                    // Emit footer next poll.
                    if !this.footer_emitted {
                        this.footer_emitted = true;
                        let footer = write_footer(&this.chunk_on_disk_lens);
                        return Poll::Ready(Some(Ok(Bytes::from(footer))));
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Decompressing stream over framed chunks.
///
/// Two modes:
///
/// - [`DecompressingStream::new`] decodes a bare data region: the caller
///   has stripped the header and the footer (it read the footer to find
///   where the chunks end).
/// - [`DecompressingStream::whole_blob`] decodes a whole blob as stored
///   (header, chunks, footer) in one pass, with no footer read up front:
///   the plaintext size from the sidecar says where the chunks end. The
///   header must match the algorithm, and what follows the last chunk must
///   be exactly the footer those chunks imply, so a truncated or corrupted
///   blob is an error rather than a short read.
pub struct DecompressingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
    algorithm: CompressionAlgorithm,
    buffer: Vec<u8>,
    inner_done: bool,
    finished: bool,
    whole: Option<WholeBlob>,
}

/// State of a whole-blob decode beyond the chunks themselves.
struct WholeBlob {
    header_checked: bool,
    /// Plaintext bytes still expected.
    remaining: u64,
    /// On-disk length of each decoded chunk, to rebuild the expected footer.
    chunk_lens: Vec<u32>,
}

impl DecompressingStream {
    pub fn new(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        algorithm: CompressionAlgorithm,
    ) -> Self {
        Self {
            inner,
            algorithm,
            buffer: Vec::new(),
            inner_done: false,
            finished: false,
            whole: None,
        }
    }

    /// Decodes a whole stored blob that holds `plaintext_len` bytes.
    pub fn whole_blob(
        inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
        algorithm: CompressionAlgorithm,
        plaintext_len: u64,
    ) -> Self {
        let mut stream = Self::new(inner, algorithm);
        stream.whole = Some(WholeBlob {
            header_checked: false,
            remaining: plaintext_len,
            chunk_lens: Vec::new(),
        });
        stream
    }

    fn try_decode_chunk(&mut self) -> Result<Option<Bytes>, io::Error> {
        if let Some(whole) = self.whole.as_mut() {
            if !whole.header_checked {
                if self.buffer.len() < HEADER_SIZE {
                    return Ok(None);
                }
                let (algorithm, _chunk_size) = format::parse_header(&self.buffer[..HEADER_SIZE])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if algorithm != self.algorithm {
                    return Err(invalid_data(format!(
                        "compression header says {algorithm:?}, expected {:?}",
                        self.algorithm
                    )));
                }
                self.buffer.drain(..HEADER_SIZE);
                whole.header_checked = true;
            }
            if whole.remaining == 0 {
                // Only the footer is left: it is checked at the end of input.
                let footer_len = whole.chunk_lens.len() * 4 + FOOTER_COUNT_SIZE;
                if self.buffer.len() > footer_len {
                    return Err(invalid_data("unexpected data after the last compressed chunk"));
                }
                return Ok(None);
            }
        }
        if self.buffer.len() < CHUNK_LEN_HEADER {
            return Ok(None);
        }
        let (compressed_len, plaintext_len) = format::parse_chunk_header(&self.buffer[..CHUNK_LEN_HEADER])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let total = CHUNK_LEN_HEADER + compressed_len as usize;
        if self.buffer.len() < total {
            return Ok(None);
        }
        if let Some(whole) = self.whole.as_ref() {
            if plaintext_len as u64 > whole.remaining {
                return Err(invalid_data("compressed chunks exceed the object size"));
            }
        }
        let compressed = &self.buffer[CHUNK_LEN_HEADER..total];
        let decoded = decompress_chunk(self.algorithm, compressed, plaintext_len as usize)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if decoded.len() != plaintext_len as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "chunk plaintext length mismatch: expected {}, got {}",
                    plaintext_len,
                    decoded.len()
                ),
            ));
        }
        self.buffer.drain(..total);
        if let Some(whole) = self.whole.as_mut() {
            whole.remaining -= plaintext_len as u64;
            whole.chunk_lens.push(total as u32);
        }
        Ok(Some(Bytes::from(decoded)))
    }

    /// Checks what is left once the input has ended.
    fn finish(&mut self) -> Result<(), io::Error> {
        let leftover = std::mem::take(&mut self.buffer);
        match self.whole.as_ref() {
            None if leftover.is_empty() => Ok(()),
            None => Err(invalid_data("truncated compressed chunk")),
            Some(whole) if !whole.header_checked || whole.remaining > 0 => {
                Err(invalid_data("truncated compressed blob"))
            }
            Some(whole) if leftover != write_footer(&whole.chunk_lens) => {
                Err(invalid_data("compressed footer does not match its chunks"))
            }
            Some(_) => Ok(()),
        }
    }
}

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

impl Stream for DecompressingStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        loop {
            match this.try_decode_chunk() {
                Ok(Some(bytes)) => return Poll::Ready(Some(Ok(bytes))),
                Ok(None) => {}
                Err(e) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(e)));
                }
            }
            if this.inner_done {
                this.finished = true;
                return Poll::Ready(this.finish().err().map(Err));
            }
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => this.buffer.extend_from_slice(&chunk),
                Poll::Ready(Some(Err(e))) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(e)));
                }
                Poll::Ready(None) => this.inner_done = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Decompresses a contiguous slice of framed chunks and returns the
/// plaintext slice corresponding to `[range_start, range_end]` (inclusive).
///
/// `chunk_data` must begin at the first chunk header to consider. The helper
/// walks chunks by their `compressed_len` prefix, so it works for any
/// framed input — independent of the footer index.
#[allow(clippy::too_many_arguments)]
pub fn decompress_range(
    algorithm: CompressionAlgorithm,
    chunk_size: u32,
    chunk_data: &[u8],
    first_chunk_index: u64,
    range_start: u64,
    range_end: u64,
) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    let mut offset = 0usize;
    let mut chunk_idx = first_chunk_index;

    while offset < chunk_data.len() {
        if offset + CHUNK_LEN_HEADER > chunk_data.len() {
            return Err("truncated chunk length prefix".to_string());
        }
        let (compressed_len, plaintext_len) =
            format::parse_chunk_header(&chunk_data[offset..offset + CHUNK_LEN_HEADER])?;
        let total = CHUNK_LEN_HEADER + compressed_len as usize;
        if offset + total > chunk_data.len() {
            return Err("truncated compressed chunk".to_string());
        }

        let chunk_pt_start = chunk_idx * chunk_size as u64;
        let chunk_pt_end = chunk_pt_start + plaintext_len as u64;

        if chunk_pt_end <= range_start || chunk_pt_start > range_end {
            offset += total;
            chunk_idx += 1;
            continue;
        }

        let compressed = &chunk_data[offset + CHUNK_LEN_HEADER..offset + total];
        let decoded = decompress_chunk(algorithm, compressed, plaintext_len as usize)?;

        let slice_start = if range_start > chunk_pt_start {
            (range_start - chunk_pt_start) as usize
        } else {
            0
        };
        let slice_end = if range_end + 1 < chunk_pt_end {
            (range_end + 1 - chunk_pt_start) as usize
        } else {
            decoded.len()
        };
        if slice_start < slice_end {
            result.extend_from_slice(&decoded[slice_start..slice_end]);
        }

        offset += total;
        chunk_idx += 1;
    }

    Ok(result)
}

/// Returns the on-disk byte offset of chunk `i`, given the per-chunk on-disk
/// lengths (as parsed from the footer).
pub fn chunk_disk_offset(chunk_lens: &[u32], chunk_index: usize) -> u64 {
    let mut off = HEADER_SIZE as u64;
    for len in chunk_lens.iter().take(chunk_index) {
        off += *len as u64;
    }
    off
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::iter as stream_iter;
    use tokio_stream::StreamExt;

    type TestStream = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

    fn bytes_to_stream(data: &[u8]) -> TestStream {
        Box::pin(stream_iter(vec![Ok(Bytes::copy_from_slice(data))]))
    }

    async fn collect(stream: TestStream) -> Vec<u8> {
        let mut s = std::pin::pin!(stream);
        let mut buf = Vec::new();
        while let Some(c) = s.as_mut().next().await {
            buf.extend_from_slice(&c.unwrap());
        }
        buf
    }

    async fn compress_bytes(algorithm: CompressionAlgorithm, chunk_size: u32, data: &[u8]) -> Vec<u8> {
        let (s, _) = CompressingStream::new(bytes_to_stream(data), algorithm, 3, chunk_size);
        collect(Box::pin(s)).await
    }

    #[tokio::test]
    async fn roundtrip_single_chunk_zstd() {
        let data = b"hello compression world";
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, format::DEFAULT_CHUNK_SIZE, data).await;
        // Validate header.
        let (alg, cs) = format::parse_header(&encoded).unwrap();
        assert_eq!(alg, CompressionAlgorithm::Zstd);
        assert_eq!(cs, format::DEFAULT_CHUNK_SIZE);
    }

    #[tokio::test]
    async fn roundtrip_multi_chunk() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let cs = 1024u32;
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, cs, &data).await;

        // Parse footer to find data region.
        let lens = format::parse_footer(&encoded).unwrap();
        assert_eq!(lens.len(), 5); // 4 full chunks + 1 partial
        let footer_size = lens.len() * 4 + format::FOOTER_COUNT_SIZE;
        let data_region = &encoded[HEADER_SIZE..encoded.len() - footer_size];

        let dec = DecompressingStream::new(bytes_to_stream(data_region), CompressionAlgorithm::Zstd);
        let decoded = collect(Box::pin(dec)).await;
        assert_eq!(decoded, data);
    }

    /// Feeds `data` in `piece`-byte slices, the way a file reader may split it.
    fn pieces_to_stream(data: &[u8], piece: usize) -> TestStream {
        let items: Vec<Result<Bytes, io::Error>> =
            data.chunks(piece).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        Box::pin(stream_iter(items))
    }

    /// Decodes a whole blob, returning the plaintext or the first error.
    async fn decode_whole(
        stream: TestStream,
        algorithm: CompressionAlgorithm,
        plaintext_len: u64,
    ) -> Result<Vec<u8>, io::Error> {
        let dec = DecompressingStream::whole_blob(stream, algorithm, plaintext_len);
        let mut s = std::pin::pin!(dec);
        let mut buf = Vec::new();
        while let Some(c) = s.as_mut().next().await {
            buf.extend_from_slice(&c?);
        }
        Ok(buf)
    }

    #[tokio::test]
    async fn whole_blob_roundtrip() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        for alg in [CompressionAlgorithm::Zstd, CompressionAlgorithm::Lz4] {
            let encoded = compress_bytes(alg, 1024, &data).await;
            let got = decode_whole(bytes_to_stream(&encoded), alg, data.len() as u64).await;
            assert_eq!(got.unwrap(), data, "{alg:?}");
        }
    }

    #[tokio::test]
    async fn whole_blob_roundtrip_in_small_pieces() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, 1024, &data).await;
        for piece in [1, 3, 9, 1000] {
            let got = decode_whole(
                pieces_to_stream(&encoded, piece),
                CompressionAlgorithm::Zstd,
                data.len() as u64,
            )
            .await;
            assert_eq!(got.unwrap(), data, "piece size {piece}");
        }
    }

    #[tokio::test]
    async fn whole_blob_empty_plaintext() {
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, 1024, b"").await;
        let got = decode_whole(bytes_to_stream(&encoded), CompressionAlgorithm::Zstd, 0).await;
        assert!(got.unwrap().is_empty());
    }

    #[tokio::test]
    async fn whole_blob_rejects_a_bad_header() {
        let data = b"some plaintext to compress".to_vec();
        let mut encoded = compress_bytes(CompressionAlgorithm::Zstd, 1024, &data).await;
        let len = data.len() as u64;
        let wrong_alg =
            decode_whole(bytes_to_stream(&encoded), CompressionAlgorithm::Lz4, len).await;
        assert!(wrong_alg.is_err());
        encoded[0] = b'X';
        let bad_magic =
            decode_whole(bytes_to_stream(&encoded), CompressionAlgorithm::Zstd, len).await;
        assert!(bad_magic.is_err());
    }

    #[tokio::test]
    async fn whole_blob_rejects_a_truncated_blob() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, 1024, &data).await;
        let len = data.len() as u64;
        // Cut inside the header, inside a chunk, and inside the footer.
        for cut in [4, HEADER_SIZE + 20, encoded.len() - 2] {
            let got = decode_whole(
                bytes_to_stream(&encoded[..cut]),
                CompressionAlgorithm::Zstd,
                len,
            )
            .await;
            assert!(got.is_err(), "cut at {cut} must fail");
        }
    }

    #[tokio::test]
    async fn whole_blob_rejects_a_footer_that_does_not_match() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, 1024, &data).await;
        let len = data.len() as u64;

        let mut wrong_index = encoded.clone();
        let index_start = encoded.len() - format::FOOTER_COUNT_SIZE - 5 * 4;
        wrong_index[index_start] ^= 0xff;
        let got = decode_whole(bytes_to_stream(&wrong_index), CompressionAlgorithm::Zstd, len);
        assert!(got.await.is_err());

        let mut trailing = encoded.clone();
        trailing.extend_from_slice(&[0u8; 16]);
        let got = decode_whole(bytes_to_stream(&trailing), CompressionAlgorithm::Zstd, len);
        assert!(got.await.is_err());
    }

    #[tokio::test]
    async fn whole_blob_rejects_a_size_that_does_not_match() {
        let data: Vec<u8> = (0..5000).map(|i| (i % 251) as u8).collect();
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, 1024, &data).await;
        for len in [4999u64, 5001, 4096, 0] {
            let got = decode_whole(bytes_to_stream(&encoded), CompressionAlgorithm::Zstd, len);
            assert!(got.await.is_err(), "declared size {len} must fail");
        }
    }

    #[tokio::test]
    async fn range_read_zstd() {
        let data = b"0123456789ABCDEF0123456789ABCDEF".to_vec();
        let cs = 8u32;
        let encoded = compress_bytes(CompressionAlgorithm::Zstd, cs, &data).await;
        let lens = format::parse_footer(&encoded).unwrap();
        let footer_size = lens.len() * 4 + format::FOOTER_COUNT_SIZE;
        let data_region = &encoded[HEADER_SIZE..encoded.len() - footer_size];

        let out =
            decompress_range(CompressionAlgorithm::Zstd, cs, data_region, 0, 6, 17).unwrap();
        assert_eq!(out, &data[6..=17]);
    }

    #[tokio::test]
    async fn empty_roundtrip() {
        let encoded = compress_bytes(CompressionAlgorithm::Snappy, 64, b"").await;
        // Header only + empty footer.
        assert_eq!(encoded.len(), HEADER_SIZE + format::FOOTER_COUNT_SIZE);
        let lens = format::parse_footer(&encoded).unwrap();
        assert!(lens.is_empty());
    }
}
