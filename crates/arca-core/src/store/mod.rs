//! Storage trait definitions.

pub mod blob;
pub mod credential;
pub mod grant;
pub mod metadata;
pub mod team;
pub mod user;

pub use blob::{BlobEncryptionInfo, BlobGetResult, BlobPutResult, BlobStore, ByteRange, ByteStream, SidecarMeta, SsecBlobOps};
pub use credential::CredentialStore;
pub use grant::GrantStore;
pub use metadata::MetadataStore;
pub use team::TeamStore;
pub use user::UserStore;
