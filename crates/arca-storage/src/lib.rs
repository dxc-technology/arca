//! arca-storage: Storage implementations for Arca.
//!
//! Provides `SqliteStore` for credential and metadata storage,
//! and `FsBlobStore` for filesystem blob storage.

pub mod fs;
pub mod sqlite;

pub use fs::FsBlobStore;
pub use sqlite::SqliteStore;
