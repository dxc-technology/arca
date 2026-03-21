//! Metrics snapshot storage trait.
//!
//! Stores periodic gauge readings for historical metrics visualization.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A periodic snapshot of server gauge metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Auto-generated row ID (populated on read, ignored on insert).
    #[serde(default)]
    pub id: i64,
    /// Timestamp of the snapshot (UTC).
    pub timestamp: DateTime<Utc>,
    /// Number of buckets.
    pub bucket_count: u64,
    /// Number of objects (latest versions only).
    pub object_count: u64,
    /// Total storage size in bytes.
    pub total_size_bytes: u64,
    /// Total disk capacity in bytes (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total_bytes: Option<u64>,
    /// Available disk space in bytes (if available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_available_bytes: Option<u64>,
    /// Number of active HTTP connections at snapshot time.
    #[serde(default)]
    pub active_connections: u64,
}

/// Trait for metrics snapshot storage operations.
#[async_trait::async_trait]
pub trait MetricsStore: Send + Sync {
    /// Insert a metrics snapshot.
    async fn insert_metrics_snapshot(
        &self,
        snapshot: &MetricsSnapshot,
    ) -> Result<(), crate::error::ArcaError>;

    /// Query metrics snapshots within a time range.
    async fn list_metrics_snapshots(
        &self,
        from: Option<DateTime<Utc>>,
        to: Option<DateTime<Utc>>,
        limit: u32,
    ) -> Result<Vec<MetricsSnapshot>, crate::error::ArcaError>;

    /// Delete metrics snapshots older than the given timestamp.
    /// Returns the number of deleted snapshots.
    async fn purge_metrics_snapshots(
        &self,
        before: DateTime<Utc>,
    ) -> Result<u64, crate::error::ArcaError>;
}
