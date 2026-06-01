//! User storage trait.

use crate::error::ArcaError;
use crate::types::User;

/// Storage operations for user management.
#[async_trait::async_trait]
pub trait UserStore: Send + Sync {
    /// Creates a new user. Fails if user_id or username already exists.
    async fn put_user(&self, user: &User) -> Result<(), ArcaError>;

    /// Gets a user by ID.
    async fn get_user(&self, user_id: &str) -> Result<Option<User>, ArcaError>;

    /// Gets a user by username.
    async fn get_user_by_username(&self, username: &str) -> Result<Option<User>, ArcaError>;

    /// Lists all users.
    async fn list_users(&self) -> Result<Vec<User>, ArcaError>;

    /// Updates a user's mutable fields. Returns false if not found.
    async fn update_user(
        &self,
        user_id: &str,
        username: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError>;

    /// Deletes a user. Returns false if not found.
    async fn delete_user(&self, user_id: &str) -> Result<bool, ArcaError>;

    /// Applies a user received verbatim from a cluster peer (Phase 29): an
    /// idempotent upsert keyed by `user_id`, preserving all fields. Unlike
    /// [`UserStore::put_user`] it never errors on an existing user.
    ///
    /// Default implementation: unsupported (non-clustered backends).
    async fn apply_remote_user(&self, _user: &User) -> Result<(), ArcaError> {
        Err(ArcaError::Internal(
            "apply_remote_user: cluster replication is not supported by this backend".to_string(),
        ))
    }
}
