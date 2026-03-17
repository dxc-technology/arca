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

    /// Updates a user's description. Returns false if not found.
    async fn update_user(&self, user_id: &str, description: &str) -> Result<bool, ArcaError>;

    /// Deletes a user. Returns false if not found.
    async fn delete_user(&self, user_id: &str) -> Result<bool, ArcaError>;
}
