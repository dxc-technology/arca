//! Shared blob garbage-collection core: selects and reclaims orphan blob files.
//!
//! An orphan blob is an on-disk blob file that no live object row, in-progress
//! multipart part, or non-orphan composite sidecar references. They arise from
//! interrupted uploads, overwrites, crashes between the metadata and blob
//! delete, and blob-delete failures (which are logged and swallowed by the
//! delete handlers because the metadata row is already gone — the object is
//! deleted from S3's point of view, and reclaiming the bytes is this module's
//! job, not the request's).
//!
//! The selection is **composite-aware** (a composite's part blobs are kept
//! alive only while the composite blob is still metadata-referenced) and
//! **fail-safe** (any enumeration error aborts the pass so nothing is deleted
//! on a partially-computed referenced set). It is shared by three callers: the
//! cluster anti-entropy worker, and the offline `arca gc` CLI (and any future
//! single-node maintenance worker).

use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime};

use arca_core::cluster::plan_blob_gc;
use arca_core::error::ArcaError;
use arca_core::store::{MetadataStore, RawBlobOps};
use arca_core::types::BlobId;

/// Outcome of a reclaim scan: how many on-disk blobs were examined and which of
/// them are safe to reclaim.
#[derive(Debug)]
pub struct ReclaimPlan {
    /// Total on-disk blob files scanned.
    pub scanned: usize,
    /// The subset safe to reclaim (unreferenced and grace-expired).
    pub candidates: Vec<BlobId>,
}

/// Computes the on-disk blob files safe to reclaim: those NOT referenced and
/// older than `grace`, plus the total scanned count for reporting.
///
/// Composite-aware (data-loss guard): a composite blob has no file of its own;
/// its parts are referenced only by its sidecar, so this unions in the part
/// blob_ids of every composite whose composite blob is still
/// metadata-referenced. Parts of an *orphaned* composite (its object row gone)
/// are intentionally left reclaimable.
///
/// Fail-safe: if ANY enumeration (referenced ids, sidecar list, a sidecar read,
/// the on-disk list) fails, this returns `Err` and the caller reclaims nothing.
pub async fn collect_reclaimable_blobs(
    metadata: &dyn MetadataStore,
    raw: &dyn RawBlobOps,
    grace: Duration,
) -> Result<ReclaimPlan, ArcaError> {
    // 1) Blobs referenced by metadata (live object rows + in-progress parts).
    let mut referenced: HashSet<BlobId> =
        metadata.list_referenced_blob_ids().await?.into_iter().collect();

    // 2) Add the part blobs of every NON-orphan composite sidecar (a composite
    // is non-orphan iff its own blob is still metadata-referenced).
    for sid in raw.list_sidecar_ids().await? {
        if let Some(meta) = raw.read_sidecar(&sid).await? {
            if let Some(parts) = meta.composite {
                if referenced.contains(&sid) {
                    for p in parts {
                        referenced.insert(p.blob_id);
                    }
                }
            }
        }
    }

    // 3) On-disk blob files minus referenced, grace-expired.
    let on_disk = raw.list_blob_ids().await?;
    let candidates = plan_blob_gc(&on_disk, &referenced, SystemTime::now(), grace);
    Ok(ReclaimPlan {
        scanned: on_disk.len(),
        candidates,
    })
}

/// Reclaims orphan blob files (collect + delete). Returns the number of files
/// reclaimed.
///
/// Emits a concise per-run INFO summary: one line when the pass starts and one
/// when it completes (with scanned/candidates/reclaimed/failed/elapsed), so a
/// scheduled run is visible even when it reclaims nothing. A pass skipped by the
/// fail-safe (an enumeration error) is logged at WARN.
///
/// Fail-safe: on any enumeration error nothing is deleted and `0` is returned.
/// An error deleting an individual file is logged and skipped so one bad file
/// does not abort the rest of the pass.
pub async fn reclaim_blobs(
    metadata: &dyn MetadataStore,
    raw: &dyn RawBlobOps,
    grace: Duration,
) -> u64 {
    let started = Instant::now();
    tracing::info!(grace_seconds = grace.as_secs(), "blob GC pass started");

    let plan = match collect_reclaimable_blobs(metadata, raw, grace).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                error = %e,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "blob GC pass skipped (enumeration failed; nothing deleted)"
            );
            return 0;
        }
    };

    let mut reclaimed = 0u64;
    let mut failed = 0u64;
    for id in &plan.candidates {
        match raw.delete_blob_file(id).await {
            Ok(()) => reclaimed += 1,
            Err(e) => {
                failed += 1;
                tracing::warn!(error = %e, blob_id = %id.0, "blob GC: delete failed");
            }
        }
    }

    tracing::info!(
        scanned = plan.scanned,
        candidates = plan.candidates.len(),
        reclaimed,
        failed,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "blob GC pass complete"
    );
    reclaimed
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::blob::{BlobGetResult, ByteStream, SidecarMeta};

    /// Minimal `RawBlobOps` whose on-disk enumeration fails, to exercise the
    /// fail-safe path. Every other method is unreachable in these tests;
    /// `delete_blob_file` panics so a regression that deletes on a failed
    /// enumeration is caught loudly.
    struct FailingRaw;

    #[async_trait::async_trait]
    impl RawBlobOps for FailingRaw {
        async fn read_raw(&self, _: &BlobId) -> Result<BlobGetResult, ArcaError> {
            unreachable!()
        }
        async fn write_raw(&self, _: &BlobId, _: ByteStream) -> Result<u64, ArcaError> {
            unreachable!()
        }
        async fn exists(&self, _: &BlobId) -> Result<bool, ArcaError> {
            unreachable!()
        }
        async fn write_sidecar(&self, _: &BlobId, _: &SidecarMeta) -> Result<(), ArcaError> {
            unreachable!()
        }
        async fn read_sidecar(&self, _: &BlobId) -> Result<Option<SidecarMeta>, ArcaError> {
            unreachable!()
        }
        async fn list_blob_ids(&self) -> Result<Vec<(BlobId, SystemTime)>, ArcaError> {
            Err(ArcaError::Internal("on-disk enumeration boom".to_string()))
        }
        async fn list_sidecar_ids(&self) -> Result<Vec<BlobId>, ArcaError> {
            Ok(vec![])
        }
        async fn delete_blob_file(&self, _: &BlobId) -> Result<(), ArcaError> {
            panic!("delete_blob_file must never run when an enumeration failed");
        }
    }

    async fn empty_metadata() -> std::sync::Arc<dyn MetadataStore> {
        std::sync::Arc::new(arca_storage::SqliteStore::open_in_memory().await.unwrap())
    }

    #[tokio::test]
    async fn collect_is_fail_safe_on_enumeration_error() {
        let m = empty_metadata().await;
        let err = collect_reclaimable_blobs(m.as_ref(), &FailingRaw, Duration::ZERO)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ArcaError::Internal(_)),
            "an enumeration error must propagate as Err"
        );
    }

    #[tokio::test]
    async fn reclaim_deletes_nothing_when_enumeration_fails() {
        let m = empty_metadata().await;
        // FailingRaw::delete_blob_file panics if reached, so this asserts the
        // fail-safe by construction: reclaim must return 0 without deleting.
        let reclaimed = reclaim_blobs(m.as_ref(), &FailingRaw, Duration::ZERO).await;
        assert_eq!(reclaimed, 0);
    }

    fn one_chunk(data: &[u8]) -> ByteStream {
        let b = bytes::Bytes::copy_from_slice(data);
        Box::pin(futures_util::stream::once(async move {
            Ok::<_, std::io::Error>(b)
        }))
    }

    #[tokio::test]
    async fn plan_reports_scanned_and_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let fs = arca_storage::FsBlobStore::new(dir.path().join("blobs"), 2)
            .await
            .unwrap();
        // Empty metadata → every on-disk blob is unreferenced. Two blobs, grace
        // 0 → both scanned, both candidates. (Referenced-blob exclusion and the
        // grace window are covered by the anti-entropy reclaim tests.)
        let m = empty_metadata().await;
        fs.write_raw(&BlobId("aaaa".into()), one_chunk(b"x")).await.unwrap();
        fs.write_raw(&BlobId("bbbb".into()), one_chunk(b"y")).await.unwrap();

        let plan = collect_reclaimable_blobs(m.as_ref(), &fs, Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(plan.scanned, 2, "both on-disk blobs are scanned");
        let mut got: Vec<String> = plan.candidates.iter().map(|b| b.0.clone()).collect();
        got.sort();
        assert_eq!(got, vec!["aaaa".to_string(), "bbbb".to_string()]);
    }
}
