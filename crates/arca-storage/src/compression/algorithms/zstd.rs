//! Zstd codec (C bindings via the `zstd` crate).

use std::cell::RefCell;

pub fn compress(plaintext: &[u8], level: i32) -> Result<Vec<u8>, String> {
    zstd::stream::encode_all(plaintext, level).map_err(|e| format!("zstd encode: {e}"))
}

thread_local! {
    /// One decompression context per thread, reused across chunks: creating
    /// and freeing a context per chunk cost more CPU than decoding a small
    /// chunk. A chunk is one whole frame, so no state carries over.
    static DECOMPRESSOR: RefCell<Option<zstd::bulk::Decompressor<'static>>> =
        const { RefCell::new(None) };
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
