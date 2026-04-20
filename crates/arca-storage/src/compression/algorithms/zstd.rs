//! Zstd codec (C bindings via the `zstd` crate).

pub fn compress(plaintext: &[u8], level: i32) -> Result<Vec<u8>, String> {
    zstd::stream::encode_all(plaintext, level).map_err(|e| format!("zstd encode: {e}"))
}

pub fn decompress(compressed: &[u8], _plaintext_len: usize) -> Result<Vec<u8>, String> {
    zstd::stream::decode_all(compressed).map_err(|e| format!("zstd decode: {e}"))
}
