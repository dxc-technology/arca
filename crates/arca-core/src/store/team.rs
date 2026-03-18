//! Team storage trait.

use crate::error::ArcaError;
use crate::types::{Team, User};

/// Storage operations for team management.
#[async_trait::async_trait]
pub trait TeamStore: Send + Sync {
    /// Creates a new team. Fails if team_id or name already exists.
    async fn put_team(&self, team: &Team) -> Result<(), ArcaError>;

    /// Gets a team by ID.
    async fn get_team(&self, team_id: &str) -> Result<Option<Team>, ArcaError>;

    /// Lists all teams.
    async fn list_teams(&self) -> Result<Vec<Team>, ArcaError>;

    /// Updates a team's mutable fields. Returns false if not found.
    async fn update_team(
        &self,
        team_id: &str,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<bool, ArcaError>;

    /// Deletes a team and its memberships/grant attachments. Returns false if not found.
    async fn delete_team(&self, team_id: &str) -> Result<bool, ArcaError>;

    /// Adds a user to a team. No-op if already a member.
    async fn add_member(&self, team_id: &str, user_id: &str) -> Result<(), ArcaError>;

    /// Removes a user from a team. Returns false if not a member.
    async fn remove_member(&self, team_id: &str, user_id: &str) -> Result<bool, ArcaError>;

    /// Lists all members of a team.
    async fn list_members(&self, team_id: &str) -> Result<Vec<User>, ArcaError>;

    /// Lists all teams a user belongs to.
    async fn list_user_teams(&self, user_id: &str) -> Result<Vec<Team>, ArcaError>;
}
