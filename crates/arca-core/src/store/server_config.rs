//! Server-level configuration storage trait.
//!
//! Instance-wide settings that can be managed from the console when not
//! locked by the TOML config file. Follows the same key-value pattern
//! as `bucket_config`, but scoped to the server instance.

/// Trait for server-level configuration storage.
#[async_trait::async_trait]
pub trait ServerConfigStore: Send + Sync {
    /// Gets a server configuration value by key.
    async fn get_server_config(
        &self,
        key: &str,
    ) -> Result<Option<String>, crate::error::ArcaError>;

    /// Sets a server configuration value (upsert).
    async fn set_server_config(
        &self,
        key: &str,
        value: &str,
    ) -> Result<(), crate::error::ArcaError>;

    /// Deletes a server configuration value. Returns true if it existed.
    async fn delete_server_config(
        &self,
        key: &str,
    ) -> Result<bool, crate::error::ArcaError>;

    /// Lists all server configuration key-value pairs.
    async fn list_server_config(
        &self,
    ) -> Result<Vec<(String, String)>, crate::error::ArcaError>;
}
