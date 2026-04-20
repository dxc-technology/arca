//! Brotli codec (pure Rust via the `brotli` crate).

use std::io::{Read, Write};

pub fn compress(plaintext: &[u8], level: i32) -> Result<Vec<u8>, String> {
    let quality = level.clamp(0, 11) as u32;
    // Window size 22 is the default used by the `brotli` CLI.
    let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, quality, 22);
    encoder
        .write_all(plaintext)
        .map_err(|e| format!("brotli encode: {e}"))?;
    encoder.flush().map_err(|e| format!("brotli flush: {e}"))?;
    Ok(encoder.into_inner())
}

pub fn decompress(compressed: &[u8], _plaintext_len: usize) -> Result<Vec<u8>, String> {
    let mut reader = brotli::Decompressor::new(compressed, 4096);
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| format!("brotli decode: {e}"))?;
    Ok(out)
}
