//! Cluster node identity.
//!
//! Each node derives and persists its own stable `node_id` on first start
//! (mirroring [`crate::credential::ensure_root_credential`]). The id lives in
//! `server_config`, NOT in the TOML config: the cluster configuration is
//! byte-identical on every node, so identity must be self-assigned. This keeps
//! the cluster fully symmetric — any node can be (re)started from the same
//! config and will keep its own identity across restarts.

use arca_core::store::server_config::ServerConfigStore;

/// `server_config` key under which the persisted node identity is stored.
/// Defined in `arca-core` (re-exported here) because BOTH replication sides
/// must denylist it: the sender decorator and the receive handler (D12.1).
pub use arca_core::cluster::NODE_ID_KEY;

/// Returns this node's stable identity, generating and persisting a fresh UUID
/// on first start and reusing it on every subsequent start.
///
/// The value is stored in `server_config`, so it survives restarts and config
/// changes. It doubles as the `x-amz-arca-replication-source` loop-prevention
/// identity, giving every node in a cluster a distinct, stable source id with
/// zero per-node configuration.
pub async fn ensure_node_id(store: &dyn ServerConfigStore) -> anyhow::Result<String> {
    if let Some(existing) = store.get_server_config(NODE_ID_KEY).await? {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            tracing::debug!(node_id = %trimmed, "Loaded persisted cluster node identity");
            return Ok(trimmed.to_string());
        }
    }
    let node_id = uuid::Uuid::new_v4().to_string();
    store.set_server_config(NODE_ID_KEY, &node_id).await?;
    tracing::info!(node_id = %node_id, "Generated and persisted cluster node identity");
    Ok(node_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::error::ArcaError;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal in-memory `ServerConfigStore` for testing identity logic in
    /// isolation (no SQLite/Postgres needed).
    #[derive(Default)]
    struct MemStore {
        map: Mutex<HashMap<String, String>>,
    }

    #[async_trait::async_trait]
    impl ServerConfigStore for MemStore {
        async fn get_server_config(&self, key: &str) -> Result<Option<String>, ArcaError> {
            Ok(self.map.lock().unwrap().get(key).cloned())
        }
        async fn set_server_config(&self, key: &str, value: &str) -> Result<(), ArcaError> {
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), value.to_string());
            Ok(())
        }
        async fn delete_server_config(&self, key: &str) -> Result<bool, ArcaError> {
            Ok(self.map.lock().unwrap().remove(key).is_some())
        }
        async fn list_server_config(&self) -> Result<Vec<(String, String)>, ArcaError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect())
        }
    }

    #[tokio::test]
    async fn generates_and_persists_on_first_start() {
        let store = MemStore::default();
        let id = ensure_node_id(&store).await.unwrap();
        assert!(!id.is_empty());
        // Persisted under the node_id key so it survives a restart.
        let stored = store.get_server_config(NODE_ID_KEY).await.unwrap();
        assert_eq!(stored.as_deref(), Some(id.as_str()));
    }

    #[tokio::test]
    async fn reuses_existing_identity_across_restarts() {
        let store = MemStore::default();
        let first = ensure_node_id(&store).await.unwrap();
        let second = ensure_node_id(&store).await.unwrap();
        assert_eq!(first, second, "node id must be stable across restarts");
    }

    #[tokio::test]
    async fn regenerates_when_blank() {
        let store = MemStore::default();
        store.set_server_config(NODE_ID_KEY, "   ").await.unwrap();
        let id = ensure_node_id(&store).await.unwrap();
        assert!(!id.trim().is_empty());
        assert_ne!(id.trim(), "");
    }

    #[tokio::test]
    async fn distinct_nodes_self_assign_distinct_ids() {
        let a = MemStore::default();
        let b = MemStore::default();
        let id_a = ensure_node_id(&a).await.unwrap();
        let id_b = ensure_node_id(&b).await.unwrap();
        assert_ne!(id_a, id_b, "independent nodes must get distinct identities");
    }
}
