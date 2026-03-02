//! arca-core: Shared types, traits, and error types for the Arca S3 server.
//!
//! This crate has zero I/O dependencies and is independently testable.

pub mod error;
pub mod s3;
pub mod store;
pub mod types;

pub use error::{ArcaError, S3Error, S3ErrorCode};
pub use s3::bucket_name::validate_bucket_name;
pub use types::{BlobId, BucketInfo, Credential, ObjectRecord};
