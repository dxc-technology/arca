//! arca-storage: Storage implementations for Arca.
//!
//! Provides `SqliteStore` for credential and metadata storage,
//! and `FsBlobStore` for filesystem blob storage.

pub mod caching;
pub mod encrypted_blob;
pub mod encryption;
pub mod fs;
#[cfg(feature = "postgres")]
pub mod pg;
pub mod sqlite;
pub mod ssec_blob;

pub use caching::CachingMetadataStore;
pub use encrypted_blob::EncryptingBlobStore;
pub use fs::FsBlobStore;
#[cfg(feature = "postgres")]
pub use pg::PgStore;
pub use sqlite::SqliteStore;
pub use ssec_blob::SsecBlobStore;
