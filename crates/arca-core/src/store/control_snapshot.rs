//! Control-plane snapshot/reconcile storage trait (Phase 29 HA, decision 12).
//!
//! A node periodically pulls a peer's [`ControlSnapshot`], merges it
//! last-writer-wins via [`crate::cluster::plan_control_merge`], and applies the
//! resulting [`ControlMergePlan`] here. This is what lets a node that was down
//! during control-plane changes (a credential revoked, a bucket deleted) catch
//! up without resurrecting deleted entities.
//!
//! `apply_*_at` preserve the SOURCE's `updated_at` (no `now()` re-stamp) so the
//! same timestamp converges across nodes and the merge reaches a fixed point
//! instead of flapping.

use chrono::{DateTime, Utc};

use crate::cluster::{ControlMergePlan, ControlSnapshot};
use crate::error::ArcaError;
use crate::types::{Credential, Team, User};

/// Reads/writes the full control-plane state for cluster reconcile.
#[async_trait::async_trait]
pub trait ControlSnapshotStore: Send + Sync {
    /// Reads this node's entire control plane (the tombstoned families:
    /// credentials, users, teams, grants, buckets) plus all tombstones.
    async fn build_control_snapshot(&self) -> Result<ControlSnapshot, ArcaError>;

    /// Upserts a credential, preserving the given `updated_at` verbatim (LWW key).
    async fn apply_credential_at(
        &self,
        credential: &Credential,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError>;

    /// Upserts a user, preserving the given `updated_at` verbatim.
    async fn apply_user_at(
        &self,
        user: &User,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError>;

    /// Upserts a team, preserving the given `updated_at` verbatim.
    async fn apply_team_at(
        &self,
        team: &Team,
        updated_at: DateTime<Utc>,
    ) -> Result<(), ArcaError>;

    /// Applies the IDENTITY part of a computed merge plan: credential/user/team
    /// upserts (with preserved timestamps), grant upserts, the corresponding
    /// deletes, and tombstone adopt/clear. Grants reuse their verbatim upsert
    /// (their `updated_at` lives in the struct).
    ///
    /// Buckets are NOT applied here — the caller applies `plan.upsert_buckets` /
    /// `plan.delete_buckets` through its `MetadataStore` handle so the metadata
    /// cache is invalidated (the concrete store would bypass it).
    async fn apply_control_merge(&self, plan: &ControlMergePlan) -> Result<(), ArcaError>;
}
