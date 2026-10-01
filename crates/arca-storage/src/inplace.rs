//! Crash-safe in-place rewrite of a blob together with its sidecar.
//!
//! Used by the OFFLINE tools (`compress-existing`, `decompress-existing`,
//! `encrypt-existing`, `decrypt-existing`) that replace the bytes of a blob that
//! is already durable and describe the new bytes in its `.meta` sidecar. Those
//! two files cannot be replaced by one atomic operation, and the window between
//! the two renames is dangerous: with the new blob and the old sidecar the
//! object is unreadable, and for encryption the new sidecar holds the only copy
//! of the wrapped DEK. This module closes that window with a journal.
//!
//! # Protocol
//!
//! 1. The caller writes the new blob to [`rewrite_tmp_path`] and fsyncs it.
//! 2. [`commit_rewrite`] writes the new sidecar durably to
//!    [`pending_sidecar_path`] (the journal),
//! 3. renames the temp blob over the blob and fsyncs the directory,
//! 4. renames the journal over the sidecar and fsyncs the directory.
//!
//! Whatever the crash point, [`resolve_pending`] (run by the tools before they
//! look at a sidecar) restores a consistent pair. The journal is durable before
//! the blob rename and the blob rename is durable before the journal rename, so
//! the only surviving states are:
//!
//! * no journal: the old pair, or the new pair. Any temp blob is discarded.
//! * journal + temp blob still present: the blob rename did not happen, so the
//!   old pair is intact. Discard both.
//! * journal + no temp blob: the blob rename happened (same-directory rename is
//!   atomic across a crash), roll forward by renaming the journal over the
//!   sidecar.
//!
//! The decision never inspects blob contents, so it is exact. Both leftover
//! names end in `.tmp`: GC and `fsck` already ignore or report that suffix and
//! never delete it. The journal of an encryption rewrite holds the only copy of
//! the new wrapped DEK until it is rolled forward, so it must not be deleted by
//! hand.
//!
//! fsync cannot be observed from a test; the ordering and the recovery
//! decision are unit-tested instead.

use std::io;
use std::path::{Path, PathBuf};

use arca_core::error::ArcaError;
use arca_core::store::blob::SidecarMeta;

use crate::fs::{rename_durable, sync_dir, write_file_atomic};

/// Temp file the new blob bytes must be written (and fsynced) to.
pub fn rewrite_tmp_path(blob: &Path) -> PathBuf {
    append_suffix(blob, ".rewrite.tmp")
}

/// Journal holding the new sidecar until the rewrite is fully committed.
pub fn pending_sidecar_path(meta_path: &Path) -> PathBuf {
    append_suffix(meta_path, ".pending.tmp")
}

fn append_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// The blob file that belongs to `meta_path` (`<id>.meta` -> `<id>`).
fn blob_of(meta_path: &Path) -> io::Result<PathBuf> {
    let name = meta_path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".meta"))
        .ok_or_else(|| io::Error::other(format!("not a sidecar path: {}", meta_path.display())))?;
    Ok(meta_path.with_file_name(name))
}

/// What [`resolve_pending`] found.
#[derive(Debug)]
pub enum PendingOutcome {
    /// Nothing was left over.
    Clean,
    /// An interrupted rewrite never replaced the blob: the old pair is intact
    /// and the leftovers were removed.
    Discarded,
    /// An interrupted rewrite had replaced the blob: the journal was committed
    /// as the sidecar. Carries the sidecar now in force.
    RolledForward(SidecarMeta),
}

/// Replaces `blob` with the already fsynced `rewrite_tmp_path(blob)` and
/// `meta_path` with `new_meta`, durably and crash-safely (see the module docs).
/// On an error before the blob is replaced the old pair is untouched and the
/// temp files are removed. If the error comes after, the journal is kept and
/// the next [`resolve_pending`] rolls forward.
pub async fn commit_rewrite(
    blob: &Path,
    meta_path: &Path,
    new_meta: &SidecarMeta,
) -> Result<(), ArcaError> {
    let json = serde_json::to_string(new_meta)
        .map_err(|e| ArcaError::Internal(format!("serialize sidecar: {e}")))?;
    let (blob, meta_path) = (blob.to_path_buf(), meta_path.to_path_buf());
    tokio::task::spawn_blocking(move || commit_rewrite_sync(&blob, &meta_path, json.as_bytes()))
        .await
        .map_err(|e| ArcaError::Internal(format!("rewrite join: {e}")))?
        .map_err(|e| ArcaError::Internal(format!("in-place rewrite: {e}")))
}

fn commit_rewrite_sync(blob: &Path, meta_path: &Path, json: &[u8]) -> io::Result<()> {
    let tmp = rewrite_tmp_path(blob);
    let pending = pending_sidecar_path(meta_path);

    // Journal first: durable before the blob is touched.
    if let Err(e) = write_file_atomic(&pending, json) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, blob) {
        // The blob was not replaced (rename is atomic): drop everything.
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(&pending);
        return Err(e);
    }
    // From here the journal is never dropped, not even if this fsync fails.
    if let Some(parent) = blob.parent() {
        sync_dir(parent)?;
    }
    // Blob replaced: the journal now MUST reach the sidecar (or be rolled
    // forward later), never be dropped.
    rename_durable(&pending, meta_path)
}

/// Repairs the leftovers of an interrupted [`commit_rewrite`] for the sidecar at
/// `meta_path`. Offline only: the server must be stopped.
pub async fn resolve_pending(meta_path: &Path) -> Result<PendingOutcome, ArcaError> {
    let meta_path = meta_path.to_path_buf();
    tokio::task::spawn_blocking(move || resolve_pending_sync(&meta_path))
        .await
        .map_err(|e| ArcaError::Internal(format!("resolve join: {e}")))?
        .map_err(|e| ArcaError::Internal(format!("resolving interrupted rewrite: {e}")))
}

fn resolve_pending_sync(meta_path: &Path) -> io::Result<PendingOutcome> {
    let blob = blob_of(meta_path)?;
    let tmp = rewrite_tmp_path(&blob);
    let pending = pending_sidecar_path(meta_path);

    if !pending.exists() {
        // No journal: a temp blob is a partial write of a rewrite that never
        // committed.
        return match std::fs::remove_file(&tmp) {
            Ok(()) => Ok(PendingOutcome::Discarded),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(PendingOutcome::Clean),
            Err(e) => Err(e),
        };
    }
    if tmp.exists() {
        std::fs::remove_file(&tmp)?;
        std::fs::remove_file(&pending)?;
        return Ok(PendingOutcome::Discarded);
    }
    let meta: SidecarMeta = serde_json::from_slice(&std::fs::read(&pending)?)
        .map_err(|e| io::Error::other(format!("journal {}: {e}", pending.display())))?;
    rename_durable(&pending, meta_path)?;
    Ok(PendingOutcome::RolledForward(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(etag: &str) -> SidecarMeta {
        SidecarMeta {
            bucket: "b".into(),
            key: "k".into(),
            size: 3,
            etag: etag.into(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: Default::default(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    /// Old pair: blob "old" + sidecar etag "old". Returns (blob, meta_path).
    fn fixture(dir: &Path) -> (PathBuf, PathBuf) {
        let blob = dir.join("abc");
        let meta_path = dir.join("abc.meta");
        std::fs::write(&blob, b"old").unwrap();
        std::fs::write(&meta_path, serde_json::to_string(&meta("old")).unwrap()).unwrap();
        (blob, meta_path)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn etag(meta_path: &Path) -> String {
        let m: SidecarMeta = serde_json::from_slice(&std::fs::read(meta_path).unwrap()).unwrap();
        m.etag
    }

    #[tokio::test]
    async fn commit_replaces_both_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let (blob, meta_path) = fixture(dir.path());
        std::fs::write(rewrite_tmp_path(&blob), b"new").unwrap();

        commit_rewrite(&blob, &meta_path, &meta("new")).await.unwrap();

        assert_eq!(std::fs::read(&blob).unwrap(), b"new");
        assert_eq!(etag(&meta_path), "new");
        assert_eq!(names(dir.path()), vec!["abc", "abc.meta"]);
    }

    #[tokio::test]
    async fn failed_journal_write_leaves_old_pair_and_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let (blob, meta_path) = fixture(dir.path());
        std::fs::write(rewrite_tmp_path(&blob), b"new").unwrap();
        // Make the journal write fail: its path is a non-empty directory.
        let pending = pending_sidecar_path(&meta_path);
        std::fs::create_dir(&pending).unwrap();
        std::fs::write(pending.join("keep"), b"x").unwrap();

        assert!(commit_rewrite(&blob, &meta_path, &meta("new")).await.is_err());

        assert_eq!(std::fs::read(&blob).unwrap(), b"old");
        assert_eq!(etag(&meta_path), "old");
        assert_eq!(names(dir.path()), vec!["abc", "abc.meta", "abc.meta.pending.tmp"]);
    }

    #[tokio::test]
    async fn resolve_is_clean_when_nothing_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let (_blob, meta_path) = fixture(dir.path());
        assert!(matches!(
            resolve_pending(&meta_path).await.unwrap(),
            PendingOutcome::Clean
        ));
    }

    #[tokio::test]
    async fn resolve_discards_partial_temp_blob_without_journal() {
        // Crash while the new blob was being written.
        let dir = tempfile::tempdir().unwrap();
        let (blob, meta_path) = fixture(dir.path());
        std::fs::write(rewrite_tmp_path(&blob), b"par").unwrap();

        assert!(matches!(
            resolve_pending(&meta_path).await.unwrap(),
            PendingOutcome::Discarded
        ));
        assert_eq!(std::fs::read(&blob).unwrap(), b"old");
        assert_eq!(etag(&meta_path), "old");
        assert_eq!(names(dir.path()), vec!["abc", "abc.meta"]);
    }

    #[tokio::test]
    async fn resolve_discards_when_crash_hit_before_the_blob_rename() {
        // Journal written, temp blob still there: blob not replaced.
        let dir = tempfile::tempdir().unwrap();
        let (blob, meta_path) = fixture(dir.path());
        std::fs::write(rewrite_tmp_path(&blob), b"new").unwrap();
        std::fs::write(
            pending_sidecar_path(&meta_path),
            serde_json::to_string(&meta("new")).unwrap(),
        )
        .unwrap();

        assert!(matches!(
            resolve_pending(&meta_path).await.unwrap(),
            PendingOutcome::Discarded
        ));
        assert_eq!(std::fs::read(&blob).unwrap(), b"old");
        assert_eq!(etag(&meta_path), "old");
        assert_eq!(names(dir.path()), vec!["abc", "abc.meta"]);
    }

    #[tokio::test]
    async fn resolve_rolls_forward_when_crash_hit_between_the_two_renames() {
        // Blob already replaced (no temp blob), sidecar still old.
        let dir = tempfile::tempdir().unwrap();
        let (blob, meta_path) = fixture(dir.path());
        std::fs::write(&blob, b"new").unwrap();
        std::fs::write(
            pending_sidecar_path(&meta_path),
            serde_json::to_string(&meta("new")).unwrap(),
        )
        .unwrap();

        match resolve_pending(&meta_path).await.unwrap() {
            PendingOutcome::RolledForward(m) => assert_eq!(m.etag, "new"),
            other => panic!("expected roll-forward, got {other:?}"),
        }
        assert_eq!(std::fs::read(&blob).unwrap(), b"new");
        assert_eq!(etag(&meta_path), "new");
        assert_eq!(names(dir.path()), vec!["abc", "abc.meta"]);
    }
}
