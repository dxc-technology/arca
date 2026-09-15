//! arca-core: Shared types, traits, and error types for the Arca S3 server.
//!
//! This crate has zero I/O dependencies and is independently testable.
//!
//! Contains AI-generated code: see the NOTICE file at the repository root.

pub mod cluster;
pub mod credential;
pub mod error;
pub mod policy;
pub mod s3;
pub mod store;
pub mod types;

pub use error::{ArcaError, S3Error, S3ErrorCode};
pub use policy::{Effect, Evaluation, PolicyDocument, Statement};
pub use s3::bucket_name::validate_bucket_name;
pub use store::{AuditEntry, AuditFilter, AuditStore, MetricsSnapshot, MetricsStore, ServerConfigStore};
pub use types::{
    BlobId, BucketInfo, Credential, Grant, ListBucketResultParams, ListEntry,
    MultipartUploadRecord, ObjectRecord, PartRecord, StorageStats, Team, User, VersioningState,
};
