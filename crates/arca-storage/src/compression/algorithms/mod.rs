//! Per-algorithm chunk codecs. Each submodule exposes `compress` + `decompress`
//! operating on a single chunk of bytes.

pub mod brotli;
pub mod gzip;
pub mod lz4;
pub mod snappy;
pub mod xz;
pub mod zstd;
