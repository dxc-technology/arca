//! Replication journal storage trait.
//!
//! The journal persists every pending/in-flight replication action so that
//! retries survive restarts and a destination outage. The replicator worker
//! drains the journal periodically, updating each entry's status on every
//! delivery attempt.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ArcaError;

/// What triggered a journal entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationEventType {
    /// PutObject or CompleteMultipartUpload — replicate full object data.
    Put,
    /// Delete marker creation on a versioned bucket.
    DeleteMarker,
    /// Object-tagging change (PutObjectTagging / DeleteObjectTagging).
    Tag,
}

impl ReplicationEventType {
    pub fn as_db(self) -> &'static str {
        match self {
            ReplicationEventType::Put => "put",
            ReplicationEventType::DeleteMarker => "delete_marker",
            ReplicationEventType::Tag => "tag",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "put" => Some(ReplicationEventType::Put),
            "delete_marker" => Some(ReplicationEventType::DeleteMarker),
            "tag" => Some(ReplicationEventType::Tag),
            _ => None,
        }
    }
}

/// A single row in the replication journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub id: String,
    pub bucket: String,
    pub key: String,
    pub version_id: Option<String>,
    pub rule_id: String,
    /// "put" | "delete_marker" | "tag"
    pub event_type: String,
    pub destination_endpoint: String,
    pub destination_bucket: String,
    /// "pending" | "in_flight" | "completed" | "failed"
    pub status: String,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub next_retry_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Query filter for `list_journal`.
#[derive(Debug, Default, Clone)]
pub struct JournalFilter {
    pub bucket: Option<String>,
    pub status: Option<String>,
    pub rule_id: Option<String>,
    pub offset: u32,
    pub limit: u32,
}

/// Replication journal store.
#[async_trait::async_trait]
pub trait ReplicationStore: Send + Sync {
    /// Insert a new pending journal entry.
    async fn insert_journal_entry(&self, entry: &JournalEntry) -> Result<(), ArcaError>;

    /// Claim up to `limit` entries whose `status in ('pending','failed')` and
    /// `next_retry_at <= now`. Claimed rows are flipped to `in_flight`.
    async fn claim_batch(&self, limit: u32) -> Result<Vec<JournalEntry>, ArcaError>;

    /// Update the status of a journal entry and its `attempts`, `last_error`,
    /// and `next_retry_at` fields atomically.
    async fn update_status(
        &self,
        id: &str,
        status: &str,
        attempts: u32,
        last_error: Option<&str>,
        next_retry_at: DateTime<Utc>,
    ) -> Result<(), ArcaError>;

    /// List journal entries for the admin API.
    async fn list_journal(&self, filter: &JournalFilter) -> Result<Vec<JournalEntry>, ArcaError>;

    /// Count rows matching a filter.
    async fn count_journal(&self, filter: &JournalFilter) -> Result<u64, ArcaError>;

    /// Delete journal entries where `status = 'completed' AND updated_at < before`.
    /// Returns the number of deleted rows.
    async fn purge_completed(&self, before: DateTime<Utc>) -> Result<u64, ArcaError>;

    /// Delete journal entries where `updated_at < before` regardless of status
    /// (the hard cap that keeps the table bounded even with a permanent outage).
    async fn purge_all_older(&self, before: DateTime<Utc>) -> Result<u64, ArcaError>;

    /// Set the `replication_status` column on the matching object version.
    /// Passing `None` clears the column.
    async fn set_object_replication_status(
        &self,
        bucket: &str,
        key: &str,
        version_id: Option<&str>,
        status: Option<&str>,
    ) -> Result<(), ArcaError>;
}
