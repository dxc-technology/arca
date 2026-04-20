//! On-disk framed format for transparently compressed blobs.
//!
//! ```text
//! [4-byte magic: "ACMP"]
//! [1-byte version: 0x01]
//! [1-byte algorithm code]
//! [4-byte chunk_size: LE u32]                (plaintext chunk size)
//! -- chunks --
//! [chunk 0: 4-byte LE compressed_len | 4-byte LE plaintext_len | compressed bytes]
//! [chunk 1: ...]
//! ...
//! -- footer (chunk index) --
//! [n x u32 LE: on-disk length of each chunk including its 8-byte length header]
//! [u32 LE: chunk count n]
//! ```
//!
//! The per-chunk plaintext length is stored explicitly so the last (partial)
//! chunk and any algorithm that produces variable-size output both decode
//! correctly. The footer index is a cheap way to seek to chunk `i` in O(1)
//! for ranged reads.

use arca_core::store::CompressionAlgorithm;

pub const MAGIC: &[u8; 4] = b"ACMP";
pub const VERSION: u8 = 0x01;
pub const DEFAULT_CHUNK_SIZE: u32 = 64 * 1024;

/// File header size: 4 (magic) + 1 (version) + 1 (algorithm) + 4 (chunk_size).
pub const HEADER_SIZE: usize = 10;

/// Per-chunk length prefix: 4 (compressed_len) + 4 (plaintext_len).
pub const CHUNK_LEN_HEADER: usize = 8;

/// Footer fixed size: trailing `u32` chunk count.
pub const FOOTER_COUNT_SIZE: usize = 4;

/// Builds the file header.
pub fn write_header(algorithm: CompressionAlgorithm, chunk_size: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(HEADER_SIZE);
    buf.extend_from_slice(MAGIC);
    buf.push(VERSION);
    buf.push(algorithm.code());
    buf.extend_from_slice(&chunk_size.to_le_bytes());
    buf
}

/// Parses the file header. Returns `(algorithm, chunk_size)`.
pub fn parse_header(data: &[u8]) -> Result<(CompressionAlgorithm, u32), &'static str> {
    if data.len() < HEADER_SIZE {
        return Err("compression header too short");
    }
    if &data[0..4] != MAGIC {
        return Err("invalid compression magic bytes");
    }
    if data[4] != VERSION {
        return Err("unsupported compression format version");
    }
    let algorithm = CompressionAlgorithm::from_code(data[5])
        .ok_or("unknown compression algorithm code")?;
    let chunk_size = u32::from_le_bytes([data[6], data[7], data[8], data[9]]);
    if chunk_size == 0 {
        return Err("chunk_size must be > 0");
    }
    Ok((algorithm, chunk_size))
}

/// Encodes a per-chunk prefix (compressed_len, plaintext_len).
pub fn write_chunk_header(compressed_len: u32, plaintext_len: u32) -> [u8; CHUNK_LEN_HEADER] {
    let mut buf = [0u8; CHUNK_LEN_HEADER];
    buf[0..4].copy_from_slice(&compressed_len.to_le_bytes());
    buf[4..8].copy_from_slice(&plaintext_len.to_le_bytes());
    buf
}

/// Parses a per-chunk prefix.
pub fn parse_chunk_header(data: &[u8]) -> Result<(u32, u32), &'static str> {
    if data.len() < CHUNK_LEN_HEADER {
        return Err("truncated chunk header");
    }
    let compressed_len = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let plaintext_len = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    Ok((compressed_len, plaintext_len))
}

/// Builds the footer chunk-index from the list of on-disk chunk lengths
/// (each length already includes the 8-byte per-chunk header).
pub fn write_footer(chunk_on_disk_lens: &[u32]) -> Vec<u8> {
    let n = chunk_on_disk_lens.len() as u32;
    let mut buf = Vec::with_capacity(chunk_on_disk_lens.len() * 4 + FOOTER_COUNT_SIZE);
    for len in chunk_on_disk_lens {
        buf.extend_from_slice(&len.to_le_bytes());
    }
    buf.extend_from_slice(&n.to_le_bytes());
    buf
}

/// Parses the trailing footer and returns per-chunk on-disk lengths in order.
///
/// `tail` must end at the last byte of the file (i.e. include the count u32
/// followed by its n*u32 index). The function reads from the end.
pub fn parse_footer(tail: &[u8]) -> Result<Vec<u32>, &'static str> {
    if tail.len() < FOOTER_COUNT_SIZE {
        return Err("footer too short");
    }
    let count_start = tail.len() - FOOTER_COUNT_SIZE;
    let n = u32::from_le_bytes([
        tail[count_start],
        tail[count_start + 1],
        tail[count_start + 2],
        tail[count_start + 3],
    ]) as usize;
    let index_bytes = n.checked_mul(4).ok_or("footer count overflow")?;
    if tail.len() < FOOTER_COUNT_SIZE + index_bytes {
        return Err("footer index truncated");
    }
    let index_start = count_start - index_bytes;
    let mut lens = Vec::with_capacity(n);
    for i in 0..n {
        let off = index_start + i * 4;
        lens.push(u32::from_le_bytes([
            tail[off],
            tail[off + 1],
            tail[off + 2],
            tail[off + 3],
        ]));
    }
    Ok(lens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let buf = write_header(CompressionAlgorithm::Zstd, DEFAULT_CHUNK_SIZE);
        assert_eq!(buf.len(), HEADER_SIZE);
        let (alg, cs) = parse_header(&buf).unwrap();
        assert_eq!(alg, CompressionAlgorithm::Zstd);
        assert_eq!(cs, DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn header_rejects_bad_magic() {
        let mut buf = write_header(CompressionAlgorithm::Gzip, 1024);
        buf[0] = b'X';
        assert!(parse_header(&buf).is_err());
    }

    #[test]
    fn chunk_header_roundtrip() {
        let buf = write_chunk_header(123, 456);
        let (c, p) = parse_chunk_header(&buf).unwrap();
        assert_eq!(c, 123);
        assert_eq!(p, 456);
    }

    #[test]
    fn footer_roundtrip() {
        let lens = vec![100u32, 200, 300];
        let buf = write_footer(&lens);
        let got = parse_footer(&buf).unwrap();
        assert_eq!(got, lens);
    }

    #[test]
    fn footer_empty() {
        let buf = write_footer(&[]);
        let got = parse_footer(&buf).unwrap();
        assert!(got.is_empty());
    }
}
