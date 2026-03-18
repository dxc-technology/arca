//! Credential storage trait.

use crate::types::Credential;

/// Trait for credential storage operations.
#[async_trait::async_trait]
pub trait CredentialStore: Send + Sync {
    /// Stores a credential. Fails if the access_key_id already exists.
    async fn put_credential(&self, credential: &Credential) -> Result<(), crate::error::ArcaError>;

    /// Retrieves a credential by access key ID. Returns None if not found.
    async fn get_credential(
        &self,
        access_key_id: &str,
    ) -> Result<Option<Credential>, crate::error::ArcaError>;

    /// Lists all credentials (both active and inactive).
    async fn list_credentials(&self) -> Result<Vec<Credential>, crate::error::ArcaError>;

    /// Deletes a credential by access key ID. Returns true if deleted, false if not found.
    async fn delete_credential(
        &self,
        access_key_id: &str,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Sets the active flag on a credential. Returns false if not found.
    async fn set_credential_active(
        &self,
        access_key_id: &str,
        active: bool,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Counts the number of active credentials.
    async fn count_active_credentials(&self) -> Result<u64, crate::error::ArcaError>;
}
