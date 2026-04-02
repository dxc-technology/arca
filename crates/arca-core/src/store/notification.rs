//! Notification event storage trait.
//!
//! Stores notification event records for delivery tracking and the console event log.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A notification event record (persisted for delivery tracking and log viewing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationEventRecord {
    /// Auto-generated row ID (populated on read, ignored on insert).
    #[serde(default)]
    pub id: String,
    /// Bucket that generated the event.
    pub bucket: String,
    /// Object key that triggered the event.
    pub key: String,
    /// S3 event name (e.g. "s3:ObjectCreated:Put").
    pub event_name: String,
    /// When the event occurred.
    pub event_time: DateTime<Utc>,
    /// Full JSON payload (S3EventMessage serialized).
    pub payload: String,
    /// Webhook destination URL.
    pub destination_url: String,
    /// ID of the notification configuration that matched.
    pub configuration_id: String,
    /// Delivery status: "pending", "delivered", or "failed".
    pub delivery_status: String,
    /// Number of delivery attempts made.
    #[serde(default)]
    pub delivery_attempts: u32,
    /// Last delivery error message, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// When the record was created.
    pub created_at: DateTime<Utc>,
}

/// Filter criteria for querying notification events.
#[derive(Debug, Default, Clone)]
pub struct NotificationEventFilter {
    /// Filter by bucket name.
    pub bucket: Option<String>,
    /// Filter by event name (e.g. "s3:ObjectCreated:Put").
    pub event_name: Option<String>,
    /// Filter by delivery status ("pending", "delivered", "failed").
    pub delivery_status: Option<String>,
    /// Number of entries to skip (for pagination).
    pub offset: u32,
    /// Maximum number of entries to return (default: 100).
    pub limit: u32,
}

/// Trait for notification event storage operations.
#[async_trait::async_trait]
pub trait NotificationStore: Send + Sync {
    /// Insert a notification event record.
    async fn insert_notification_event(
        &self,
        event: &NotificationEventRecord,
    ) -> Result<(), crate::error::ArcaError>;

    /// Update the delivery status of a notification event.
    async fn update_notification_event_status(
        &self,
        id: &str,
        status: &str,
        attempts: u32,
        last_error: Option<&str>,
    ) -> Result<(), crate::error::ArcaError>;

    /// Query notification events matching the given filter.
    async fn list_notification_events(
        &self,
        filter: &NotificationEventFilter,
    ) -> Result<Vec<NotificationEventRecord>, crate::error::ArcaError>;

    /// Count notification events matching the given filter (for pagination metadata).
    async fn count_notification_events(
        &self,
        filter: &NotificationEventFilter,
    ) -> Result<u64, crate::error::ArcaError>;

    /// Delete notification events older than the given timestamp.
    /// Returns the number of deleted entries.
    async fn purge_notification_events(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, crate::error::ArcaError>;
}
