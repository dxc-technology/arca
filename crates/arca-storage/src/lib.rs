//! arca-storage: Storage implementations for Arca.
//!
//! Provides `SqliteStore` for credential and metadata storage.

pub mod sqlite;

pub use sqlite::SqliteStore;
