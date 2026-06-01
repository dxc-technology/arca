//! Grant (policy) storage trait.

use crate::error::ArcaError;
use crate::policy::PolicyDocument;
use crate::types::Grant;

/// Storage operations for grant (policy) management.
#[async_trait::async_trait]
pub trait GrantStore: Send + Sync {
    /// Creates a new grant. Fails if grant_id or name already exists.
    async fn put_grant(&self, grant: &Grant) -> Result<(), ArcaError>;

    /// Gets a grant by ID.
    async fn get_grant(&self, grant_id: &str) -> Result<Option<Grant>, ArcaError>;

    /// Gets a grant by name.
    async fn get_grant_by_name(&self, name: &str) -> Result<Option<Grant>, ArcaError>;

    /// Lists all grants.
    async fn list_grants(&self) -> Result<Vec<Grant>, ArcaError>;

    /// Updates a grant's name, description, and/or document. Returns false if not found.
    async fn update_grant(
        &self,
        grant_id: &str,
        name: Option<&str>,
        description: Option<&str>,
        document: Option<&PolicyDocument>,
    ) -> Result<bool, ArcaError>;

    /// Deletes a grant and all its attachments. Returns false if not found.
    async fn delete_grant(&self, grant_id: &str) -> Result<bool, ArcaError>;

    /// Attaches a grant to a user. No-op if already attached.
    async fn attach_to_user(&self, user_id: &str, grant_id: &str) -> Result<(), ArcaError>;

    /// Detaches a grant from a user. Returns false if not attached.
    async fn detach_from_user(&self, user_id: &str, grant_id: &str) -> Result<bool, ArcaError>;

    /// Attaches a grant to a team. No-op if already attached.
    async fn attach_to_team(&self, team_id: &str, grant_id: &str) -> Result<(), ArcaError>;

    /// Detaches a grant from a team. Returns false if not attached.
    async fn detach_from_team(&self, team_id: &str, grant_id: &str) -> Result<bool, ArcaError>;

    /// Lists grants directly attached to a user.
    async fn list_user_grants(&self, user_id: &str) -> Result<Vec<Grant>, ArcaError>;

    /// Lists grants attached to a team.
    async fn list_team_grants(&self, team_id: &str) -> Result<Vec<Grant>, ArcaError>;

    /// Returns all effective policy documents for a user (direct grants + team grants).
    /// This is the query used for policy evaluation on every request.
    async fn get_effective_policies(
        &self,
        user_id: &str,
    ) -> Result<Vec<PolicyDocument>, ArcaError>;

    /// Applies a grant received verbatim from a cluster peer (Phase 29): an
    /// idempotent upsert keyed by `grant_id`. Unlike [`GrantStore::put_grant`]
    /// it never errors on an existing grant. The user/team attachments
    /// replicate separately (the join-table ops are already idempotent).
    ///
    /// Default implementation: unsupported (non-clustered backends).
    async fn apply_remote_grant(&self, _grant: &Grant) -> Result<(), ArcaError> {
        Err(ArcaError::Internal(
            "apply_remote_grant: cluster replication is not supported by this backend".to_string(),
        ))
    }
}
