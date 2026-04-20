//! Snappy codec (pure Rust via `snap`).

pub fn compress(plaintext: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = snap::raw::Encoder::new();
    encoder.compress_vec(plaintext).map_err(|e| format!("snappy encode: {e}"))
}

pub fn decompress(compressed: &[u8], _plaintext_len: usize) -> Result<Vec<u8>, String> {
    let mut decoder = snap::raw::Decoder::new();
    decoder.decompress_vec(compressed).map_err(|e| format!("snappy decode: {e}"))
}
