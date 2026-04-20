//! XZ (LZMA2) codec (pure Rust via `lzma-rs`).

use std::io::Cursor;

pub fn compress(plaintext: &[u8], _level: i32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut input = Cursor::new(plaintext);
    lzma_rs::xz_compress(&mut input, &mut out).map_err(|e| format!("xz encode: {e}"))?;
    Ok(out)
}

pub fn decompress(compressed: &[u8], _plaintext_len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut input = Cursor::new(compressed);
    lzma_rs::xz_decompress(&mut input, &mut out).map_err(|e| format!("xz decode: {e}"))?;
    Ok(out)
}
