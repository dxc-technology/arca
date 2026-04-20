//! Transparent at-rest compression engine.
//!
//! Mirrors the shape of the encryption engine: a wrapper `BlobStore`
//! frames plaintext into fixed-size chunks, compresses each chunk
//! with a chosen algorithm, and stores a chunk-index footer that
//! enables O(1) seek for ranged reads.

pub mod algorithms;
pub mod auto;
pub mod codec;
pub mod format;
pub mod stream;
