//! Storage trait definitions.

pub mod audit;
pub mod blob;
pub mod connector;
pub mod control_snapshot;
pub mod control_tombstone;
pub mod credential;
pub mod grant;
pub mod maintenance;
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
pub use control_snapshot::ControlSnapshotStore;
pub use control_tombstone::{
    ControlTombstone, ControlTombstoneStore, TOMBSTONE_BUCKET, TOMBSTONE_BUCKET_CONFIG,
    TOMBSTONE_BUCKET_TAGS, TOMBSTONE_CREDENTIAL, TOMBSTONE_GRANT, TOMBSTONE_MULTIPART,
    TOMBSTONE_SERVER_CONFIG, TOMBSTONE_TEAM, TOMBSTONE_TEAM_GRANT, TOMBSTONE_TEAM_MEMBER,
    TOMBSTONE_USER, TOMBSTONE_USER_GRANT,
};
pub use credential::CredentialStore;
pub use grant::GrantStore;
pub use maintenance::{
    MaintenanceJob, MaintenanceJobLog, MaintenanceJobMode, MaintenanceJobStatus, MaintenanceStore,
    DEFAULT_MAX_JOB_LOGS,
};
pub use metadata::{DeletePrecondition, MetadataStore, WritePrecondition};
pub use metrics::{MetricsSnapshot, MetricsStore};
pub use notification::{NotificationEventFilter, NotificationEventRecord, NotificationStore};
pub use presigned_url::{PresignedUrlRecord, PresignedUrlStore};
pub use replication::{JournalEntry, JournalFilter, ReplicationEventType, ReplicationStore};
pub use server_config::ServerConfigStore;
pub use team::TeamStore;
pub use user::UserStore;
