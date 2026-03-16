//! Storage trait definitions.

pub mod blob;
pub mod credential;
pub mod metadata;

pub use blob::{BlobEncryptionInfo, BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream, SidecarMeta, SsecBlobOps};
pub use credential::CredentialStore;
pub use metadata::MetadataStore;
