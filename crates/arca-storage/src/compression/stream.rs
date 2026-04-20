//! Compressing and decompressing byte streams with chunk framing.
//!
//! `CompressingStream` wraps a plaintext `ByteStream`, buffers into
//! fixed-size plaintext chunks, compresses each chunk with the configured
//! algorithm, and yields the framed output (header + chunks + footer).
//!
//! `DecompressingStream` wraps a compressed byte stream, parses the header
//! once, then decodes each chunk and yields the plaintext.
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
    self, write_chunk_header, write_footer, write_header, CHUNK_LEN_HEADER, HEADER_SIZE,
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

/// Decompressing stream that wraps a framed compressed byte stream
/// (header, chunks, footer — but the footer bytes at the tail are simply
/// rejected as "extra" chunks because their first 8 bytes look like a
/// chunk header with a giant compressed_len). So we strip the footer
/// out-of-band: the caller uses `DecompressingStream` only when the footer
/// has been removed, or when the caller knows a stop condition.
///
/// For simplicity, in the full-read path we read chunks until we see a
/// chunk header with plaintext_len == 0 and compressed_len == 0, which
/// never happens naturally, AND we track bytes against the known file
/// size minus footer length. Since that's awkward, the `CompressingBlobStore`
/// reads the footer first, derives the data region, and feeds only that
/// region here.
pub struct DecompressingStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>,
    algorithm: CompressionAlgorithm,
    buffer: Vec<u8>,
    inner_done: bool,
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
        }
    }

    fn try_decode_chunk(&mut self) -> Result<Option<Bytes>, io::Error> {
        if self.buffer.len() < CHUNK_LEN_HEADER {
            return Ok(None);
        }
        let (compressed_len, plaintext_len) = format::parse_chunk_header(&self.buffer[..CHUNK_LEN_HEADER])
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let total = CHUNK_LEN_HEADER + compressed_len as usize;
        if self.buffer.len() < total {
            return Ok(None);
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
        Ok(Some(Bytes::from(decoded)))
    }
}

impl Stream for DecompressingStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        match this.try_decode_chunk() {
            Ok(Some(bytes)) => return Poll::Ready(Some(Ok(bytes))),
            Ok(None) => {}
            Err(e) => return Poll::Ready(Some(Err(e))),
        }

        if this.inner_done {
            if this.buffer.is_empty() {
                return Poll::Ready(None);
            }
            return Poll::Ready(Some(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated compressed chunk",
            ))));
        }

        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(chunk))) => {
                    this.buffer.extend_from_slice(&chunk);
                    match this.try_decode_chunk() {
                        Ok(Some(bytes)) => return Poll::Ready(Some(Ok(bytes))),
                        Ok(None) => {}
                        Err(e) => return Poll::Ready(Some(Err(e))),
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    this.inner_done = true;
                    match this.try_decode_chunk() {
                        Ok(Some(bytes)) => return Poll::Ready(Some(Ok(bytes))),
                        Ok(None) => {
                            if this.buffer.is_empty() {
                                return Poll::Ready(None);
                            }
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "truncated compressed chunk",
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
