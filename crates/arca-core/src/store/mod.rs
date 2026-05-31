//! Storage trait definitions.

pub mod audit;
pub mod blob;
pub mod connector;
pub mod credential;
pub mod grant;
pub mod metadata;
pub mod metrics;
pub mod notification;
pub mod presigned_url;
pub mod replication;
pub mod server_config;
pub mod team;
pub mod user;

pub use audit::{AuditEntry, AuditFilter, AuditStore};
pub use blob::{
    BlobCompressionInfo, BlobEncryptionInfo, BlobGetResult, BlobPutResult, BlobStore, ByteRange,
    ByteStream, CompositePart, CompressionAlgorithm, CompressionMetrics, CompressionSkipReason,
    PutHints, RawBlobOps, SidecarMeta, SsecBlobOps,
};
pub use connector::{ConnectorRegistry, DeliveryResult, NotificationConnector, TestResult};
pub use credential::CredentialStore;
pub use grant::GrantStore;
pub use metadata::MetadataStore;
pub use metrics::{MetricsSnapshot, MetricsStore};
pub use notification::{NotificationEventFilter, NotificationEventRecord, NotificationStore};
pub use presigned_url::{PresignedUrlRecord, PresignedUrlStore};
pub use replication::{JournalEntry, JournalFilter, ReplicationEventType, ReplicationStore};
pub use server_config::ServerConfigStore;
pub use team::TeamStore;
pub use user::UserStore;
