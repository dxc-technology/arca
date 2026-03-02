//! Storage trait definitions.

pub mod blob;
pub mod credential;
pub mod metadata;

pub use blob::{BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream, SidecarMeta};
pub use credential::CredentialStore;
pub use metadata::MetadataStore;
