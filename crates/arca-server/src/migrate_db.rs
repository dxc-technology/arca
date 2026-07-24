//! Offline `arca migrate-db` (Phase 30, milestone M3).
//!
//! Copies ALL metadata from the configured backend (the SOURCE) into the OTHER
//! backend (the TARGET), in place. Blob files are filesystem-resident and are
//! NOT touched. After a successful run the operator switches `metadata_backend`
//! in the config (and `[storage.postgres]` as needed) and restarts onto the new
//! backend.
//!
//! This is the disaster-recovery / scripted escape hatch for the equivalent
//! online `migrate-db` maintenance job: it runs with the server STOPPED so no
//! writes race the copy. The target backend must be empty unless `--force` is
//! given (which deletes every destination row first).

use std::sync::Arc;

use anyhow::{Context, Result};

use arca_storage::migration::{self, Backend};
use arca_storage::{PgStore, SqliteStore};

use crate::config::{Config, StorageConfig};

/// Opens a [`Backend`] for the requested `backend` name from the given storage
/// config. `sqlite` uses `data_dir`; `postgres` uses `[storage.postgres]`.
pub async fn open_backend_from_storage(
    storage: &StorageConfig,
    backend: &str,
) -> Result<Backend> {
    match backend {
        "sqlite" => {
            let store = SqliteStore::open(&storage.db_path())
                .await
                .context("opening SQLite store")?;
            Ok(Backend::Sqlite(Arc::new(store)))
        }
        "postgres" => {
            let pg = storage.postgres.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "[storage.postgres] section required to use the postgres backend (set connection_string)"
                )
            })?;
            let store = PgStore::open(&pg.connection_string, pg.max_connections)
                .await
                .context("opening PostgreSQL store")?;
            Ok(Backend::Pg(Arc::new(store)))
        }
        other => anyhow::bail!("unknown backend \"{other}\" (expected \"sqlite\" or \"postgres\")"),
    }
}

/// Opens a [`Backend`] for the requested `backend` name from the given config.
pub async fn open_backend(config: &Config, backend: &str) -> Result<Backend> {
    open_backend_from_storage(&config.storage, backend).await
}

/// Entry point for `arca migrate-db --to <sqlite|postgres> [--force]`.
///
/// The SOURCE is unambiguously the *other* backend of `--to`: migrating to
/// postgres reads the existing sqlite metadata, and vice versa. We do NOT use
/// the config's (auto-detected) `metadata_backend` to pick the source, because
/// the config must carry BOTH `[storage]` (sqlite source) and `[storage.postgres]`
/// (postgres target) at once, and `load_config` auto-detects postgres whenever
/// the postgres section is present.
pub async fn run_migrate_db(config: &Config, to: &str, force: bool) -> Result<()> {
    let from = match to {
        "sqlite" => "postgres",
        "postgres" => "sqlite",
        other => anyhow::bail!("unknown target backend \"{other}\" (expected sqlite|postgres)"),
    };

    println!("Opening source backend: {from}");
    let source = open_backend(config, from).await?;
    println!("Opening target backend: {to}");
    let dest = open_backend(config, to).await?;

    println!("Migrating metadata {from} -> {to} (force={force})\n");
    // TECHDEBT(TD-023): single-pass copy with no crash-safe checkpoint. A crash
    // mid-run leaves the destination partially written and the operator must
    // drop it before retrying; the reconcile below only fires if the process
    // survives to print it.
    let report = migration::migrate_all(&source, &dest, force, |idx, total, name| {
        println!("  [{}/{}] {name}", idx + 1, total);
    })
    .await
    .map_err(|e| anyhow::anyhow!(e))?;

    println!("\nPer-table row counts:");
    for t in &report.tables {
        println!("  {:<24} {:>10}", t.table, t.source_count);
    }
    println!(
        "\nDone. {} table(s), {} row(s) copied. Source and destination reconcile.",
        report.tables.len(),
        report.total_rows
    );
    println!(
        "Next: set metadata_backend = \"{to}\" in the config and restart Arca onto the new backend."
    );
    Ok(())
}
