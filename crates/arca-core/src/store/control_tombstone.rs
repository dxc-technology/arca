//! Control-plane tombstone storage trait (Phase 29 HA).
//!
//! A hard delete of a control-plane entity (credential, user, team, grant,
//! bucket) leaves a tombstone row here so the deletion converges via the
//! periodic control-snapshot reconcile and is NOT resurrected by a peer that
//! still holds the live row (the same resurrection trap the object tombstones
//! solve for the data plane — decision 8/12).
//!
//! Tombstones are reconcile-only: reads of the entities are unaffected (the
//! entity really is gone). They are GC'd after a grace window that must exceed
//! the maximum expected node downtime. Single-node deployments never write here
//! (the cluster decorators are the only callers).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ArcaError;

/// Entity-type discriminators for control-plane tombstones. Stored verbatim in
/// the `entity_type` column and matched by the reconcile pass.
pub const TOMBSTONE_CREDENTIAL: &str = "credential";
pub const TOMBSTONE_USER: &str = "user";
pub const TOMBSTONE_TEAM: &str = "team";
pub const TOMBSTONE_GRANT: &str = "grant";
pub const TOMBSTONE_BUCKET: &str = "bucket";

/// HA hardening R5 (TD-016): the families that previously replicated in real
/// time only. Two-part keys use [`crate::cluster::pair_key`]; `bucket_tags` is
/// keyed by bucket name alone (the whole tag SET is one LWW entity, matching
/// the replace-all semantics of `PutBucketTagging` and `ControlOp::BucketTags`);
/// `server_config` by config key; `multipart` by upload id (a tombstone marks a
/// Complete/Abort so a closed upload cannot resurrect, D4).
pub const TOMBSTONE_USER_GRANT: &str = "user_grant";
pub const TOMBSTONE_TEAM_GRANT: &str = "team_grant";
pub const TOMBSTONE_TEAM_MEMBER: &str = "team_member";
pub const TOMBSTONE_BUCKET_CONFIG: &str = "bucket_config";
pub const TOMBSTONE_BUCKET_TAGS: &str = "bucket_tags";
pub const TOMBSTONE_SERVER_CONFIG: &str = "server_config";
pub const TOMBSTONE_MULTIPART: &str = "multipart";

/// A record that a control-plane entity was deleted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlTombstone {
    /// One of the `TOMBSTONE_*` discriminators.
    pub entity_type: String,
    /// The entity's primary key (access_key_id / user_id / team_id / grant_id /
    /// bucket name).
    pub entity_key: String,
    /// When the entity was deleted (last-writer-wins timestamp).
    pub deleted_at: DateTime<Utc>,
}

/// Storage for control-plane deletion tombstones.
#[async_trait::async_trait]
pub trait ControlTombstoneStore: Send + Sync {
    /// Records (upserts) a tombstone for a locally deleted entity, stamping
    /// `deleted_at = now()`.
    async fn record_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<(), ArcaError>;

    /// Adopts a peer's tombstone verbatim, keeping the latest `deleted_at` on
    /// conflict (used by the reconcile pass to propagate a deletion without
    /// re-stamping its time). Distinct from [`Self::record_control_tombstone`],
    /// which stamps `now()` for a local delete.
    async fn apply_control_tombstone(
        &self,
        tombstone: &ControlTombstone,
    ) -> Result<(), ArcaError>;

    /// Lists every control-plane tombstone (feeds the control-snapshot).
    async fn list_control_tombstones(&self) -> Result<Vec<ControlTombstone>, ArcaError>;

    /// Removes a tombstone, e.g. when the entity is legitimately re-created.
    /// Returns true if a tombstone existed.
    async fn delete_control_tombstone(
        &self,
        entity_type: &str,
        entity_key: &str,
    ) -> Result<bool, ArcaError>;

    /// Garbage-collects tombstones older than `before`. Returns how many were
    /// removed.
    async fn purge_control_tombstones(&self, before: DateTime<Utc>) -> Result<u64, ArcaError>;
}
