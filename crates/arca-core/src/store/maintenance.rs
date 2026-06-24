//! Maintenance jobs storage trait (Phase 30).
//!
//! Long-running, operator-launched maintenance operations — re-encryption,
//! metadata-backend migration, topology migration — are tracked as rows in a
//! `maintenance_jobs` table so their progress survives restarts and is
//! observable from the console. A single worker processes one job at a time and
//! commits progress per item, so pause / cancel / restart are always safe and a
//! re-launched job resumes naturally from the persisted state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ArcaError;

/// Default cap on retained log lines per job (oldest pruned on append).
pub const DEFAULT_MAX_JOB_LOGS: u32 = 500;

/// Lifecycle state of a maintenance job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceJobStatus {
    /// Created, not yet picked up by the worker.
    Pending,
    /// Actively being processed by the worker.
    Running,
    /// Paused by the operator; resumable.
    Paused,
    /// Finished successfully.
    Completed,
    /// Stopped on error (or interrupted by a restart).
    Failed,
    /// Cancelled by the operator.
    Cancelled,
}

impl MaintenanceJobStatus {
    pub fn as_db(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "paused" => Some(Self::Paused),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// A terminal state never changes again (completed / failed / cancelled).
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// An active job occupies the single-job slot (pending / running / paused),
    /// so a new job cannot be started while one exists.
    pub fn is_active(self) -> bool {
        !self.is_terminal()
    }
}

/// Whether a job runs against live S3 traffic or drains the S3 API first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceJobMode {
    /// Runs with live S3 traffic (copy-on-write + rate limit; zero downtime).
    Live,
    /// Drains the S3 API to 503 on this node for the duration (faster, quiescent).
    Maintenance,
}

impl MaintenanceJobMode {
    pub fn as_db(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Maintenance => "maintenance",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "live" => Some(Self::Live),
            "maintenance" => Some(Self::Maintenance),
            _ => None,
        }
    }
}

/// A maintenance job row.
///
/// `status` and `mode` are stored as their wire/DB strings (parsed via
/// [`MaintenanceJobStatus`] / [`MaintenanceJobMode`]) so the JSON the console
/// reads maps 1:1 to the column values, matching the replication journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceJob {
    pub id: String,
    /// Job type discriminator: "noop" | "encrypt" | "decrypt" | "migrate-db" |
    /// "migrate-topology". Kept as a string so adding a type needs no schema or
    /// enum churn; the worker dispatches on it.
    pub job_type: String,
    /// "pending" | "running" | "paused" | "completed" | "failed" | "cancelled".
    pub status: String,
    /// "live" | "maintenance".
    pub mode: String,
    /// Job-specific parameters (e.g. bucket/prefix filter, target algorithm).
    pub params: serde_json::Value,
    /// Total work units (objects, rows, …); 0 until computed.
    pub total: u64,
    /// Work units completed so far.
    pub done: u64,
    /// Throughput in job-defined units per second (e.g. objects/s or bytes/s).
    pub rate: f64,
    /// Last error, set when the job fails.
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the worker first moved the job to `running`.
    pub started_at: Option<DateTime<Utc>>,
    /// When the job reached a terminal state.
    pub finished_at: Option<DateTime<Utc>>,
}

/// A single bounded log line attached to a job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceJobLog {
    pub job_id: String,
    pub ts: DateTime<Utc>,
    /// "info" | "warn" | "error".
    pub level: String,
    pub message: String,
}

/// Storage for maintenance jobs and their logs.
#[async_trait::async_trait]
pub trait MaintenanceStore: Send + Sync {
    /// Inserts a new job (caller sets `status = pending`). Errors if `id` exists.
    async fn create_job(&self, job: &MaintenanceJob) -> Result<(), ArcaError>;

    /// Returns a job by id, or `None`.
    async fn get_job(&self, id: &str) -> Result<Option<MaintenanceJob>, ArcaError>;

    /// The single active (pending / running / paused) job, if any. The worker
    /// processes at most one job at a time and the create path refuses a new job
    /// while this returns `Some`.
    async fn active_job(&self) -> Result<Option<MaintenanceJob>, ArcaError>;

    /// Job history, newest first, capped at `limit`.
    async fn list_jobs(&self, limit: u32) -> Result<Vec<MaintenanceJob>, ArcaError>;

    /// Updates the progress counters and rate; stamps `updated_at`.
    async fn update_job_progress(
        &self,
        id: &str,
        done: u64,
        total: u64,
        rate: f64,
    ) -> Result<(), ArcaError>;

    /// Transitions a job's status, setting `last_error` (cleared with `None`).
    /// Stamps `started_at` the first time it becomes `running` and `finished_at`
    /// when it becomes terminal. Stamps `updated_at`. Returns whether a row
    /// matched.
    async fn set_job_status(
        &self,
        id: &str,
        status: MaintenanceJobStatus,
        last_error: Option<&str>,
    ) -> Result<bool, ArcaError>;

    /// Appends a log line for a job and prunes to the newest `max_logs` lines.
    async fn append_job_log(
        &self,
        job_id: &str,
        level: &str,
        message: &str,
        max_logs: u32,
    ) -> Result<(), ArcaError>;

    /// Job logs, newest first, capped at `limit`.
    async fn list_job_logs(
        &self,
        job_id: &str,
        limit: u32,
    ) -> Result<Vec<MaintenanceJobLog>, ArcaError>;

    /// On startup, marks any `running` job as `failed` ("interrupted by
    /// restart") so the worker never silently resumes a half-run job and the
    /// operator can re-launch it (a re-launched job resumes from persisted
    /// state). Returns the number of jobs reset.
    async fn interrupt_running_jobs(&self) -> Result<u64, ArcaError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_roundtrip() {
        for s in [
            MaintenanceJobStatus::Pending,
            MaintenanceJobStatus::Running,
            MaintenanceJobStatus::Paused,
            MaintenanceJobStatus::Completed,
            MaintenanceJobStatus::Failed,
            MaintenanceJobStatus::Cancelled,
        ] {
            assert_eq!(MaintenanceJobStatus::parse(s.as_db()), Some(s));
        }
        assert_eq!(MaintenanceJobStatus::parse("bogus"), None);
    }

    #[test]
    fn status_terminal_and_active_partition() {
        assert!(MaintenanceJobStatus::Pending.is_active());
        assert!(MaintenanceJobStatus::Running.is_active());
        assert!(MaintenanceJobStatus::Paused.is_active());
        assert!(MaintenanceJobStatus::Completed.is_terminal());
        assert!(MaintenanceJobStatus::Failed.is_terminal());
        assert!(MaintenanceJobStatus::Cancelled.is_terminal());
        // is_active and is_terminal are exact complements.
        for s in [
            MaintenanceJobStatus::Pending,
            MaintenanceJobStatus::Running,
            MaintenanceJobStatus::Paused,
            MaintenanceJobStatus::Completed,
            MaintenanceJobStatus::Failed,
            MaintenanceJobStatus::Cancelled,
        ] {
            assert_ne!(s.is_active(), s.is_terminal());
        }
    }

    #[test]
    fn mode_roundtrip() {
        assert_eq!(
            MaintenanceJobMode::parse(MaintenanceJobMode::Live.as_db()),
            Some(MaintenanceJobMode::Live)
        );
        assert_eq!(
            MaintenanceJobMode::parse(MaintenanceJobMode::Maintenance.as_db()),
            Some(MaintenanceJobMode::Maintenance)
        );
        assert_eq!(MaintenanceJobMode::parse("bogus"), None);
    }
}
