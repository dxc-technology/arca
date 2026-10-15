//! Offline `arca gc`: reclaim orphaned blob files and stale temp files on a
//! single node.
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
//! server is running, or drop it to `0` when the server is stopped. The same
//! grace protects the temp files of uploads in flight; the recovery files of
//! an in-place rewrite are never touched. Unfinished multipart uploads are
//! reported, not reclaimed: their parts are live until the upload is aborted.

use std::time::Duration;

use anyhow::{Context, Result};
use arca_core::store::RawBlobOps;

use crate::blob_gc;
use crate::config::Config;

/// What a gc run found and did.
#[derive(Debug, Default)]
pub struct GcSummary {
    pub orphans: usize,
    pub temp_files: usize,
    pub reclaimed: u64,
    pub temp_reclaimed: u64,
    pub failed: u64,
    pub unfinished_uploads: usize,
    pub unfinished_bytes: u64,
}

pub async fn run_gc(config: &Config, dry_run: bool, grace: Duration, verbose: bool) -> Result<GcSummary> {
    let blobs_dir = config.storage.blobs_dir();
    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }

    println!("Opening database...");
    let metadata = crate::offline_store::open_metadata_store(config)
        .await
        .context("opening metadata database")?;
    let raw = arca_storage::FsBlobStore::new(&blobs_dir, config.storage.blob_prefix_depth)
        .await
        .context("opening blob store")?;

    println!(
        "Scanning {} for orphan blobs and stale temp files (grace {}s)...",
        blobs_dir.display(),
        grace.as_secs()
    );
    // Fail-safe: any enumeration error aborts here and nothing is deleted.
    let plan = blob_gc::collect_reclaimable_blobs(metadata.as_ref(), &raw, grace)
        .await
        .context("scanning for orphan blobs (nothing was deleted)")?;
    let mut summary = GcSummary {
        orphans: plan.candidates.len(),
        temp_files: plan.temp_files.len(),
        ..Default::default()
    };

    println!(
        "Scanned {} blob(s); {} orphan(s), {} stale temp file(s).",
        plan.scanned, summary.orphans, summary.temp_files
    );

    // Not garbage: the parts of an unfinished upload are live until the
    // upload is completed or aborted (TD-052). Shown so they are not missed.
    for bucket in metadata.list_buckets().await.context("listing buckets")? {
        for u in crate::fsck::list_unfinished_uploads(metadata.as_ref(), &bucket.name).await? {
            summary.unfinished_uploads += 1;
            summary.unfinished_bytes += u.bytes;
        }
    }

    if verbose {
        for id in &plan.candidates {
            println!("  orphan {}", id.0);
        }
        for path in &plan.temp_files {
            println!("  temp {}", path.display());
        }
    }

    if plan.candidates.is_empty() && plan.temp_files.is_empty() {
        println!("Nothing to reclaim.");
    } else if dry_run {
        println!(
            "DRY RUN: {} orphan blob(s) and {} stale temp file(s) would be reclaimed. Re-run with --reclaim to delete them.",
            summary.orphans, summary.temp_files
        );
    } else {
        for id in &plan.candidates {
            match raw.delete_blob_file(id).await {
                Ok(()) => summary.reclaimed += 1,
                Err(e) => {
                    summary.failed += 1;
                    eprintln!("WARNING: failed to delete blob {}: {}", id.0, e);
                }
            }
        }
        let (deleted, failed) = blob_gc::delete_temp_files(&raw, &plan.temp_files).await;
        summary.temp_reclaimed = deleted;
        summary.failed += failed;
        println!(
            "Reclaimed {} orphan blob(s) and {} stale temp file(s){}.",
            summary.reclaimed,
            summary.temp_reclaimed,
            if summary.failed > 0 { format!("; {} failed", summary.failed) } else { String::new() }
        );
    }

    if summary.unfinished_uploads > 0 {
        println!(
            "{} unfinished multipart upload(s) hold {}: not reclaimed, they are live until aborted. \
             Abort them with arca fsck --repair --abort-uploads-older-than <days>, or add an \
             AbortIncompleteMultipartUpload lifecycle rule.",
            summary.unfinished_uploads,
            crate::fsck::human_bytes(summary.unfinished_bytes)
        );
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::MetadataStore;
    use arca_core::types::{BlobId, MultipartUploadRecord, PartRecord};
    use std::collections::HashMap;

    struct Fixture {
        _tmp: tempfile::TempDir,
        config: Config,
        store: arca_storage::SqliteStore,
        fs: arca_storage::FsBlobStore,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str(&format!(
            "[server]\nbind = \"127.0.0.1\"\nport = 9000\n\n[storage]\ndata_dir = \"{}\"\n",
            tmp.path().display()
        ))
        .unwrap();
        let store = arca_storage::SqliteStore::open(&config.storage.db_path()).await.unwrap();
        store.create_bucket("bucket").await.unwrap();
        let fs = arca_storage::FsBlobStore::new(&config.storage.blobs_dir(), 2).await.unwrap();
        Fixture { _tmp: tmp, config, store, fs }
    }

    fn write(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"bytes").unwrap();
    }

    #[tokio::test]
    async fn dry_run_counts_orphans_and_temp_files_and_deletes_nothing() {
        let f = fixture().await;
        let orphan = f.fs.blob_path(&BlobId("aaaa".into()));
        let tmp = orphan.with_file_name("bbbb.tmp");
        write(&orphan);
        write(&tmp);

        let s = run_gc(&f.config, true, Duration::ZERO, false).await.unwrap();
        assert_eq!((s.orphans, s.temp_files, s.reclaimed, s.temp_reclaimed), (1, 1, 0, 0));
        assert!(orphan.exists() && tmp.exists());
    }

    #[tokio::test]
    async fn reclaim_deletes_orphans_and_old_temp_files_only() {
        let f = fixture().await;
        let orphan = f.fs.blob_path(&BlobId("aaaa".into()));
        let tmp = orphan.with_file_name("bbbb.tmp");
        let rewrite = orphan.with_file_name("cccc.rewrite.tmp");
        write(&orphan);
        write(&tmp);
        write(&rewrite);

        let young = run_gc(&f.config, false, Duration::from_secs(3600), false).await.unwrap();
        assert_eq!((young.reclaimed, young.temp_reclaimed), (0, 0), "all younger than the grace");

        let s = run_gc(&f.config, false, Duration::ZERO, false).await.unwrap();
        assert_eq!((s.reclaimed, s.temp_reclaimed, s.failed), (1, 1, 0));
        assert!(!orphan.exists() && !tmp.exists());
        assert!(rewrite.exists());
    }

    #[tokio::test]
    async fn unfinished_uploads_are_reported_and_kept() {
        let f = fixture().await;
        f.store
            .create_multipart_upload(&MultipartUploadRecord {
                upload_id: "up".into(),
                bucket: "bucket".into(),
                key: "backup.tar".into(),
                content_type: None,
                initiated_at: chrono::Utc::now(),
                metadata: HashMap::new(),
                checksum_algorithm: None,
            })
            .await
            .unwrap();
        let part = BlobId("dddd".into());
        write(&f.fs.blob_path(&part));
        f.store
            .put_part(&PartRecord {
                upload_id: "up".into(),
                part_number: 1,
                blob_id: part.clone(),
                size: 5,
                etag: "e".into(),
                checksum_value: None,
                last_modified: None,
            })
            .await
            .unwrap();

        let s = run_gc(&f.config, false, Duration::ZERO, false).await.unwrap();
        assert_eq!((s.unfinished_uploads, s.unfinished_bytes, s.reclaimed), (1, 5, 0));
        assert!(f.fs.blob_path(&part).exists());
    }
}
