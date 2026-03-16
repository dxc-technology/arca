//! arca-storage: Storage implementations for Arca.
//!
//! Provides `SqliteStore` for credential and metadata storage,
//! and `FsBlobStore` for filesystem blob storage.

pub mod encrypted_blob;
pub mod encryption;
pub mod fs;
pub mod sqlite;
pub mod ssec_blob;

pub use encrypted_blob::EncryptingBlobStore;
pub use fs::FsBlobStore;
pub use sqlite::SqliteStore;
pub use ssec_blob::SsecBlobStore;
