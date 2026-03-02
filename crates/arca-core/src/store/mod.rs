//! Storage trait definitions.

pub mod blob;
pub mod credential;
pub mod metadata;

pub use blob::BlobStore;
pub use credential::CredentialStore;
pub use metadata::MetadataStore;
