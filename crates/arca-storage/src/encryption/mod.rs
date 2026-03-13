//! Server-side encryption engine (SSE-S3).
//!
//! Implements AES-256-GCM envelope encryption with per-object DEKs
//! wrapped by a master KEK.

pub mod format;
pub mod keys;
pub mod stream;
