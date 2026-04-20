//! Gzip codec (pure Rust via `flate2` with the miniz_oxide backend).

use std::io::{Read, Write};

pub fn compress(plaintext: &[u8], level: i32) -> Result<Vec<u8>, String> {
    let level = level.clamp(0, 9) as u32;
    let mut encoder =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(level));
    encoder.write_all(plaintext).map_err(|e| format!("gzip encode: {e}"))?;
    encoder.finish().map_err(|e| format!("gzip finish: {e}"))
}

pub fn decompress(compressed: &[u8], _plaintext_len: usize) -> Result<Vec<u8>, String> {
    let mut decoder = flate2::read::GzDecoder::new(compressed);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(|e| format!("gzip decode: {e}"))?;
    Ok(out)
}
