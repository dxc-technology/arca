//! Zstd codec (C bindings via the `zstd` crate).

use std::cell::RefCell;

thread_local! {
    /// One compression and one decompression context per thread, reused
    /// across chunks: creating, sizing and freeing a context per chunk cost
    /// more CPU than coding a small chunk. A chunk is one whole frame, so no
    /// state carries over.
    static COMPRESSOR: RefCell<Option<zstd::bulk::Compressor<'static>>> =
        const { RefCell::new(None) };
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
}

/// Encodes one chunk as a standalone frame. The frame records the chunk's
/// size, so the context sizes its tables to the chunk, not to a stream of
/// unknown length.
pub fn compress(plaintext: &[u8], level: i32) -> Result<Vec<u8>, String> {
    COMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(
                zstd::bulk::Compressor::new(level).map_err(|e| format!("zstd context: {e}"))?,
            );
        }
        let compressor = slot.as_mut().expect("context created above");
        compressor
            .set_compression_level(level)
            .map_err(|e| format!("zstd level {level}: {e}"))?;
        compressor
            .compress(plaintext)
            .map_err(|e| format!("zstd encode: {e}"))
    })
}

/// Decodes one chunk, which must expand to at most `plaintext_len` bytes.
pub fn decompress(compressed: &[u8], plaintext_len: usize) -> Result<Vec<u8>, String> {
    DECOMPRESSOR.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(
                zstd::bulk::Decompressor::new().map_err(|e| format!("zstd context: {e}"))?,
            );
        }
        let decompressor = slot.as_mut().expect("context created above");
        decompressor
            .decompress(compressed, plaintext_len)
            .map_err(|e| format!("zstd decode: {e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn compresses_many_chunks_on_one_thread() {
        // The thread-local context is reused across chunks and levels: each
        // output must be a standalone frame any zstd decoder reads.
        for (len, level) in [(4096usize, 3), (0, 3), (1, 1), (65536, 19), (1024, 3), (17, -5)] {
            let data = sample(len);
            let compressed = compress(&data, level).unwrap();
            assert_eq!(zstd::stream::decode_all(&compressed[..]).unwrap(), data);
            assert_eq!(decompress(&compressed, len).unwrap(), data);
        }
    }

    #[test]
    fn the_level_of_each_call_applies() {
        let data: Vec<u8> = b"the quick brown fox jumps over the lazy dog "
            .iter()
            .cycle()
            .take(65536)
            .copied()
            .collect();
        let fast = compress(&data, -50).unwrap();
        let strong = compress(&data, 19).unwrap();
        let fast_again = compress(&data, -50).unwrap();
        assert!(strong.len() < fast.len(), "{} vs {}", strong.len(), fast.len());
        assert_eq!(fast_again, fast);
    }

    #[test]
    fn decodes_frames_of_the_streaming_encoder() {
        // Blobs written before the context was reused hold encode_all frames.
        let data = sample(65536);
        let compressed = zstd::stream::encode_all(&data[..], 3).unwrap();
        assert_eq!(decompress(&compressed, data.len()).unwrap(), data);
    }

    #[test]
    fn decodes_many_chunks_on_one_thread() {
        // The thread-local context is reused: every chunk must still decode
        // on its own, whatever came before it.
        for len in [0usize, 1, 1024, 65536, 17, 4096] {
            let data = sample(len);
            let compressed = compress(&data, 3).unwrap();
            assert_eq!(decompress(&compressed, len).unwrap(), data);
        }
    }

    #[test]
    fn a_failed_decode_does_not_poison_the_context() {
        let data = sample(4096);
        let compressed = compress(&data, 3).unwrap();
        assert!(decompress(&compressed[..compressed.len() / 2], 4096).is_err());
        assert!(decompress(b"not a zstd frame", 4096).is_err());
        assert_eq!(decompress(&compressed, 4096).unwrap(), data);
    }

    #[test]
    fn rejects_a_chunk_larger_than_its_declared_length() {
        let data = sample(4096);
        let compressed = compress(&data, 3).unwrap();
        assert!(decompress(&compressed, 4095).is_err());
    }
}
