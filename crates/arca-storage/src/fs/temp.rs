//! Classification of the temporary files found in the blobs directory.
//!
//! Every writer publishes its file with a rename from a `*.tmp` sibling, so a
//! `*.tmp` file left behind is either still being written or was abandoned by
//! an interrupted write. The in-place rewrite tools are the exception: their
//! `*.rewrite.tmp` / `*.pending.tmp` files are the recovery state of an
//! unfinished rewrite, which re-running the same command completes, so no
//! reclaimer may ever touch them.

use crate::inplace::{PENDING_TMP_SUFFIX, REWRITE_TMP_SUFFIX};

/// What a temporary file in the blobs directory is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempFileKind {
    /// A blob temp (`<blob>.tmp`) or a sidecar temp (`<blob>.meta.<uuid>.tmp`):
    /// partial bytes nothing will ever read, reclaimable once old enough to
    /// no longer be in flight.
    Reclaimable,
    /// The recovery state of an in-place rewrite (`encrypt-existing`,
    /// `compress-existing`, ...): owned by that command, never reclaimed.
    RewriteRecovery,
}

/// Classifies a file name from the blobs directory; `None` if it is not a
/// temporary file.
pub fn temp_file_kind(name: &str) -> Option<TempFileKind> {
    if name.ends_with(REWRITE_TMP_SUFFIX) || name.ends_with(PENDING_TMP_SUFFIX) {
        Some(TempFileKind::RewriteRecovery)
    } else if name.ends_with(".tmp") {
        Some(TempFileKind::Reclaimable)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn blob_and_sidecar_temps_are_reclaimable() {
        assert_eq!(temp_file_kind(&format!("{ID}.tmp")), Some(TempFileKind::Reclaimable));
        assert_eq!(
            temp_file_kind(&format!("{ID}.meta.0123456789abcdef0123456789abcdef.tmp")),
            Some(TempFileKind::Reclaimable)
        );
    }

    #[test]
    fn rewrite_recovery_files_are_never_reclaimable() {
        assert_eq!(
            temp_file_kind(&format!("{ID}.rewrite.tmp")),
            Some(TempFileKind::RewriteRecovery)
        );
        assert_eq!(
            temp_file_kind(&format!("{ID}.meta.pending.tmp")),
            Some(TempFileKind::RewriteRecovery)
        );
    }

    #[test]
    fn blobs_and_sidecars_are_not_temp_files() {
        assert_eq!(temp_file_kind(ID), None);
        assert_eq!(temp_file_kind(&format!("{ID}.meta")), None);
    }

    #[test]
    fn suffixes_match_the_paths_the_rewrite_tools_write() {
        let blob = std::path::Path::new("/b").join(ID);
        let rewrite = crate::inplace::rewrite_tmp_path(&blob);
        let pending = crate::inplace::pending_sidecar_path(&blob.with_extension("meta"));
        for p in [rewrite, pending] {
            let name = p.file_name().unwrap().to_str().unwrap();
            assert_eq!(temp_file_kind(name), Some(TempFileKind::RewriteRecovery), "{name}");
        }
    }
}
