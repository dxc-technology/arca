//! Opening the configured metadata store from an offline CLI tool.
//!
//! Offline tools (`fsck`, `gc`, `encrypt-existing`, ...) open the store with
//! the always-durable constructors (`SqliteStore::open`, `PgStore::open`),
//! never the server's `[storage] fsync` setting. For SQLite they refuse a
//! missing database file: opening it would silently create an empty database,
//! and a tool comparing it against the blob directory would then see every
//! blob as unreferenced. The store runs in the node's mode (`[cluster]
//! enabled`), like the server's.

use std::sync::Arc;

use anyhow::Result;
use arca_core::store::MetadataStore;
use arca_storage::migration::Backend;

use crate::config::Config;
use crate::migrate_db::open_backend_from_storage;

/// Opens the metadata store of the configured `metadata_backend`.
pub async fn open_metadata_store(config: &Config) -> Result<Arc<dyn MetadataStore>> {
    let backend = config.storage.metadata_backend.as_str();
    if backend == "sqlite" {
        let db_path = config.storage.db_path();
        if !db_path.exists() {
            anyhow::bail!("Database does not exist: {}", db_path.display());
        }
    }
    // The same mode as the server on this node: a tool writing next to a
    // running cluster node must tombstone its deletes and, on PostgreSQL, take
    // the same write gate (the commit-ordered seq counter) as the server.
    let cluster_enabled = config.cluster.as_ref().is_some_and(|c| c.enabled);
    Ok(match open_backend_from_storage(&config.storage, backend).await? {
        Backend::Sqlite(store) => {
            store.set_cluster_mode(cluster_enabled);
            store as Arc<dyn MetadataStore>
        }
        Backend::Pg(store) => {
            store.set_cluster_mode(cluster_enabled);
            store as Arc<dyn MetadataStore>
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn config(data_dir: &Path, storage_extra: &str) -> Config {
        toml::from_str(&format!(
            "[server]\nbind = \"127.0.0.1\"\nport = 9000\n\n[storage]\ndata_dir = \"{}\"\n{storage_extra}",
            data_dir.display()
        ))
        .unwrap()
    }

    #[tokio::test]
    async fn opens_an_existing_sqlite_database() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(tmp.path(), "");
        arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap()
            .create_bucket("b")
            .await
            .unwrap();

        let store = open_metadata_store(&config).await.unwrap();
        assert!(store.head_bucket("b").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn refuses_a_missing_sqlite_database_without_creating_it() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(tmp.path(), "");

        let err = open_metadata_store(&config).await.err().unwrap();
        assert!(err.to_string().contains("Database does not exist"), "{err}");
        assert!(!config.storage.db_path().exists(), "no database may be created");
    }

    #[tokio::test]
    async fn rejects_an_unknown_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(tmp.path(), "metadata_backend = \"oracle\"\n");

        let err = open_metadata_store(&config).await.err().unwrap();
        assert!(err.to_string().contains("unknown backend"), "{err}");
    }

    #[tokio::test]
    async fn postgres_without_its_section_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(tmp.path(), "metadata_backend = \"postgres\"\n");

        let err = open_metadata_store(&config).await.err().unwrap();
        assert!(err.to_string().contains("[storage.postgres]"), "{err}");
    }

    fn object(key: &str) -> arca_core::types::ObjectRecord {
        arca_core::types::ObjectRecord {
            bucket: "b".into(),
            key: key.into(),
            blob_id: arca_core::types::BlobId("blob".into()),
            size: 1,
            etag: "e".into(),
            content_type: None,
            last_modified: chrono::Utc::now(),
            metadata: std::collections::HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            parts: None,
            replication_status: None,
            lock_updated_at: None,
            content_updated_at: None,
        }
    }

    /// Whether a delete through a store opened by `open_metadata_store` left
    /// a tombstone row, which only a store in cluster mode does.
    async fn delete_leaves_a_tombstone(storage_extra: &str) -> bool {
        let tmp = tempfile::tempdir().unwrap();
        let config = config(tmp.path(), storage_extra);
        arca_storage::SqliteStore::open(&config.storage.db_path()).await.unwrap();

        let store = open_metadata_store(&config).await.unwrap();
        store.create_bucket("b").await.unwrap();
        store.put_object(&object("k")).await.unwrap();
        store.delete_object("b", "k").await.unwrap();
        store
            .list_rows_changed_since(0, 100)
            .await
            .unwrap()
            .iter()
            .any(|(_, row)| row.key == "k" && row.is_tombstone)
    }

    #[tokio::test]
    async fn a_cluster_node_store_runs_in_cluster_mode() {
        let cluster = "\n[cluster]\nenabled = true\ncluster_id = \"c\"\nsecret = \"s\"\n";
        assert!(delete_leaves_a_tombstone(cluster).await);
    }

    #[tokio::test]
    async fn a_standalone_store_runs_in_single_node_mode() {
        assert!(!delete_leaves_a_tombstone("").await);
    }
}
