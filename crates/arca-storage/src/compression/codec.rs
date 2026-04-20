//! Per-algorithm chunk encoders and decoders.
//!
//! Each supported algorithm exposes a pair of sync byte-in/byte-out functions
//! that operate on one chunk at a time. Chunk size is fixed by the file
//! header (default 64 KiB plaintext). The framing layer handles buffering
//! and per-chunk length headers so codecs stay simple.

use arca_core::store::CompressionAlgorithm;

use super::algorithms::{brotli as br, gzip as gz, lz4, snappy, xz, zstd as zs};

/// Compresses a single chunk with the chosen algorithm and level.
pub fn compress_chunk(
    algorithm: CompressionAlgorithm,
    level: i32,
    plaintext: &[u8],
) -> Result<Vec<u8>, String> {
    match algorithm {
        CompressionAlgorithm::Zstd => zs::compress(plaintext, level),
        CompressionAlgorithm::Lz4 => lz4::compress(plaintext),
        CompressionAlgorithm::Snappy => snappy::compress(plaintext),
        CompressionAlgorithm::Gzip => gz::compress(plaintext, level),
        CompressionAlgorithm::Brotli => br::compress(plaintext, level),
        CompressionAlgorithm::Xz => xz::compress(plaintext, level),
    }
}

/// Decompresses a single chunk, producing exactly `plaintext_len` bytes.
pub fn decompress_chunk(
    algorithm: CompressionAlgorithm,
    compressed: &[u8],
    plaintext_len: usize,
) -> Result<Vec<u8>, String> {
    match algorithm {
        CompressionAlgorithm::Zstd => zs::decompress(compressed, plaintext_len),
        CompressionAlgorithm::Lz4 => lz4::decompress(compressed, plaintext_len),
        CompressionAlgorithm::Snappy => snappy::decompress(compressed, plaintext_len),
        CompressionAlgorithm::Gzip => gz::decompress(compressed, plaintext_len),
        CompressionAlgorithm::Brotli => br::decompress(compressed, plaintext_len),
        CompressionAlgorithm::Xz => xz::decompress(compressed, plaintext_len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(alg: CompressionAlgorithm) {
        let plaintext: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
        let compressed = compress_chunk(alg, 3, &plaintext).unwrap();
        let decoded = decompress_chunk(alg, &compressed, plaintext.len()).unwrap();
        assert_eq!(decoded, plaintext, "roundtrip failed for {alg:?}");
    }

    #[test]
    fn roundtrip_all_algorithms() {
        for alg in [
            CompressionAlgorithm::Zstd,
            CompressionAlgorithm::Lz4,
            CompressionAlgorithm::Snappy,
            CompressionAlgorithm::Gzip,
            CompressionAlgorithm::Brotli,
            CompressionAlgorithm::Xz,
        ] {
            roundtrip(alg);
        }
    }

    #[test]
    fn roundtrip_empty_chunk() {
        // A zero-byte chunk should round-trip cleanly for every codec.
        for alg in [
            CompressionAlgorithm::Zstd,
            CompressionAlgorithm::Lz4,
            CompressionAlgorithm::Snappy,
            CompressionAlgorithm::Gzip,
            CompressionAlgorithm::Brotli,
            CompressionAlgorithm::Xz,
        ] {
            let compressed = compress_chunk(alg, 3, &[]).unwrap();
            let decoded = decompress_chunk(alg, &compressed, 0).unwrap();
            assert!(decoded.is_empty(), "expected empty decode for {alg:?}");
        }
    }
}
