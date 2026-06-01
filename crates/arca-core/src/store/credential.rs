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

    /// Updates a credential's mutable fields. Returns false if not found.
    async fn update_credential(
        &self,
        access_key_id: &str,
        active: Option<bool>,
        description: Option<&str>,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Counts the number of active credentials.
    async fn count_active_credentials(&self) -> Result<u64, crate::error::ArcaError>;

    /// Applies a credential received verbatim from a cluster peer (Phase 29):
    /// an idempotent upsert keyed by `access_key_id`, preserving all fields so
    /// failover authentication recognizes the key. Unlike
    /// [`CredentialStore::put_credential`] it never errors on an existing key.
    ///
    /// Default implementation: unsupported (non-clustered backends).
    async fn apply_remote_credential(
        &self,
        _credential: &Credential,
    ) -> Result<(), crate::error::ArcaError> {
        Err(crate::error::ArcaError::Internal(
            "apply_remote_credential: cluster replication is not supported by this backend"
                .to_string(),
        ))
    }
}
