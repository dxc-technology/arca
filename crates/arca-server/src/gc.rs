//! Offline `arca gc`: reclaim orphaned blob files on a single node.
//!
//! Orphan blobs (on disk, referenced by no live object row / in-progress part /
//! non-orphan composite sidecar) accumulate from interrupted uploads,
//! overwrites, crashes between the metadata and blob delete, and swallowed
//! blob-delete failures. On a single node there is no anti-entropy worker to
//! reclaim them, so this command is the reclamation path — schedule it from
//! cron, or run it on demand.
//!
//! It reuses the shared, composite-aware, fail-safe selection in
//! [`crate::blob_gc`]. It previews by default (dry run); pass `--reclaim` to
//! actually delete. `grace` protects freshly-written blobs whose object row may
//! not be committed yet: keep it above the longest in-flight upload when the
//! server is running, or drop it to `0` when the server is stopped.

use std::time::Duration;

use anyhow::{Context, Result};
use arca_core::store::RawBlobOps;

use crate::blob_gc;
use crate::config::Config;

pub async fn run_gc(config: &Config, dry_run: bool, grace: Duration, verbose: bool) -> Result<()> {
    let blobs_dir = config.storage.blobs_dir();
    let db_path = config.storage.db_path();

    if !db_path.exists() {
        anyhow::bail!("Database does not exist: {}", db_path.display());
    }
    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }
    // Like `arca fsck`/`recover`, this offline path opens SQLite directly.
    if config.storage.metadata_backend != "sqlite" {
        anyhow::bail!(
            "arca gc currently supports only the sqlite metadata backend (found: {})",
            config.storage.metadata_backend
        );
    }

    println!("Opening database...");
    let metadata = arca_storage::SqliteStore::open(&db_path)
        .await
        .context("opening metadata database")?;
    let raw = arca_storage::FsBlobStore::new(&blobs_dir, config.storage.blob_prefix_depth)
        .await
        .context("opening blob store")?;

    println!(
        "Scanning {} for orphan blobs (grace {}s)...",
        blobs_dir.display(),
        grace.as_secs()
    );
    // Fail-safe: any enumeration error aborts here and nothing is deleted.
    let orphans = blob_gc::collect_reclaimable_blobs(&metadata, &raw, grace)
        .await
        .context("scanning for orphan blobs (nothing was deleted)")?;

    if orphans.is_empty() {
        println!("No orphan blobs to reclaim.");
        return Ok(());
    }

    if verbose {
        for id in &orphans {
            println!("  orphan {}", id.0);
        }
    }

    if dry_run {
        println!(
            "DRY RUN: {} orphan blob(s) would be reclaimed. Re-run with --reclaim to delete them.",
            orphans.len()
        );
        return Ok(());
    }

    let mut reclaimed = 0u64;
    let mut failed = 0u64;
    for id in &orphans {
        match raw.delete_blob_file(id).await {
            Ok(()) => reclaimed += 1,
            Err(e) => {
                failed += 1;
                eprintln!("WARNING: failed to delete blob {}: {}", id.0, e);
            }
        }
    }
    if failed > 0 {
        println!("Reclaimed {reclaimed} orphan blob(s); {failed} failed.");
    } else {
        println!("Reclaimed {reclaimed} orphan blob(s).");
    }
    Ok(())
}
