//! Audit log storage trait.
//!
//! Stores structured audit records for every S3 and admin operation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single audit log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// Auto-generated row ID (populated on read, ignored on insert).
    #[serde(default)]
    pub id: i64,
    /// Timestamp of the request (UTC).
    pub timestamp: DateTime<Utc>,
    /// Unique request ID (`x-amz-request-id`).
    pub request_id: String,
    /// Operation name (e.g. "PutObject", "ListBuckets", "Admin::CreateUser").
    pub operation: String,
    /// Bucket name, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// Object key, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Version ID, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_id: Option<String>,
    /// Authenticated user ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// Access key ID used for authentication.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,
    /// Client IP address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ip: Option<String>,
    /// HTTP method (GET, PUT, POST, DELETE, HEAD).
    pub http_method: String,
    /// HTTP response status code.
    pub http_status: u16,
    /// S3 error code, if the request failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Response body bytes sent.
    #[serde(default)]
    pub bytes_sent: u64,
    /// Request body bytes received.
    #[serde(default)]
    pub bytes_received: u64,
    /// Request processing duration in milliseconds.
    #[serde(default)]
    pub duration_ms: u64,
    /// Client user-agent header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

/// Filter criteria for querying audit log entries.
#[derive(Debug, Default, Clone)]
pub struct AuditFilter {
    /// Filter by bucket name.
    pub bucket: Option<String>,
    /// Filter by operation name.
    pub operation: Option<String>,
    /// Filter by user ID.
    pub user_id: Option<String>,
    /// Only entries at or after this timestamp.
    pub from: Option<DateTime<Utc>>,
    /// Only entries at or before this timestamp.
    pub to: Option<DateTime<Utc>>,
    /// Number of entries to skip (for pagination).
    pub offset: u32,
    /// Maximum number of entries to return (default: 100).
    pub limit: u32,
}

/// Trait for audit log storage operations.
#[async_trait::async_trait]
pub trait AuditStore: Send + Sync {
    /// Insert an audit log entry.
    async fn insert_audit_entry(
        &self,
        entry: &AuditEntry,
    ) -> Result<(), crate::error::ArcaError>;

    /// Insert multiple audit log entries in a single transaction.
    async fn insert_audit_entries_batch(
        &self,
        entries: &[AuditEntry],
    ) -> Result<(), crate::error::ArcaError>;

    /// Query audit log entries matching the given filter.
    async fn list_audit_entries(
        &self,
        filter: &AuditFilter,
    ) -> Result<Vec<AuditEntry>, crate::error::ArcaError>;

    /// Count audit log entries matching the given filter (for pagination metadata).
    async fn count_audit_entries(
        &self,
        filter: &AuditFilter,
    ) -> Result<u64, crate::error::ArcaError>;

    /// Delete audit log entries older than the given timestamp.
    /// Returns the number of deleted entries.
    async fn purge_audit_entries(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, crate::error::ArcaError>;
}
