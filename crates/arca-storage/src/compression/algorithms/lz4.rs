//! LZ4 codec (pure Rust via `lz4_flex`).
//!
//! Uses the raw block API — frame encapsulation is handled by our own
//! chunk framing, so we only need a single block per chunk.

pub fn compress(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    Ok(lz4_flex::block::compress(plaintext))
}

pub fn decompress(compressed: &[u8], plaintext_len: usize) -> Result<Vec<u8>, String> {
    lz4_flex::block::decompress(compressed, plaintext_len)
        .map_err(|e| format!("lz4 decode: {e}"))
}
