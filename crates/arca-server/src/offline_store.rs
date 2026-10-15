//! Opening the configured metadata store from an offline CLI tool.
//!
//! Offline tools (`fsck`, `gc`, `encrypt-existing`, ...) open the store with
//! the always-durable constructors (`SqliteStore::open`, `PgStore::open`),
//! never the server's `[storage] fsync` setting. For SQLite they refuse a
//! missing database file: opening it would silently create an empty database,
//! and a tool comparing it against the blob directory would then see every
//! blob as unreferenced.

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
    Ok(match open_backend_from_storage(&config.storage, backend).await? {
        Backend::Sqlite(store) => store as Arc<dyn MetadataStore>,
        Backend::Pg(store) => store as Arc<dyn MetadataStore>,
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
}
