//! `arca fsck --repair`: fix what a scan found, one small step at a time.
//!
//! Every fix re-verifies its item right before acting (the server may be
//! running, and the scan is a snapshot), is atomic, and is idempotent: there
//! is no journal, an interrupted run is resumed by running fsck again, which
//! rescans and finds only what is left.
//!
//! Fixes run in two phases. Database fixes first (deleting the row of a
//! version whose data is gone, aborting old unfinished uploads); if any was
//! applied the store is rescanned, so the files they left behind are found as
//! orphans by the same run. Then file fixes: stale temp files (no question
//! asked, they are partial bytes nothing reads), orphaned blobs and sidecars
//! (moved to `lost+found` unless `--delete`), and sidecars rewritten from
//! their row. On an HA cluster node only the file fixes run: deleting a row
//! there would replicate as a tombstone and delete the object on every node,
//! while anti-entropy repairs a missing blob from a peer.

use std::io::{BufRead, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

use arca_core::error::{ArcaError, S3ErrorCode};
use arca_core::store::{DeletePrecondition, MetadataStore, RawBlobOps};
use arca_core::types::BlobId;
use arca_storage::fs::rename_durable;
use arca_storage::FsBlobStore;

use super::{human_bytes, sidecar_diff, version_label, DiskFile, FsckReport, MissingBlob, SidecarMismatch, UnfinishedUpload};
use crate::blob_gc::referenced_blob_ids;

/// Options of `arca fsck --repair`.
#[derive(Debug, Clone)]
pub struct RepairOptions {
    /// Answer yes to every question.
    pub yes: bool,
    /// Delete orphaned files instead of moving them to `lost+found`.
    pub delete: bool,
    /// Offer to abort unfinished uploads initiated longer ago than this.
    pub abort_uploads_older_than: Option<chrono::Duration>,
    /// This node belongs to an HA cluster: database fixes are not offered.
    pub cluster: bool,
}

/// An answer to a repair question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    /// Yes to this item and to the rest of its category.
    All,
    /// Stop repairing.
    Quit,
}

/// Asks the operator whether to apply a fix.
pub trait Prompter {
    fn ask(&mut self, question: &str) -> Answer;
}

/// Asks on the terminal (`[y/n/a/q]`).
pub struct TtyPrompter;

impl Prompter for TtyPrompter {
    fn ask(&mut self, question: &str) -> Answer {
        let stdin = std::io::stdin();
        loop {
            print!("{question} [y/n/a/q] ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            match stdin.lock().read_line(&mut line) {
                Ok(0) | Err(_) => return Answer::Quit, // end of input
                Ok(_) => {}
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return Answer::Yes,
                "n" | "no" => return Answer::No,
                "a" | "all" => return Answer::All,
                "q" | "quit" => return Answer::Quit,
                _ => println!("  y = yes, n = no, a = yes to the rest of this category, q = stop"),
            }
        }
    }
}

/// Ctrl+C state. Outside a fix (scanning, at a question) nothing is half
/// done, so Ctrl+C exits at once; during a fix it raises `interrupted` and the
/// run stops once that fix has completed.
#[derive(Debug, Default)]
pub struct InterruptState {
    pub interrupted: AtomicBool,
    pub in_fix: AtomicBool,
}

impl InterruptState {
    /// Installs the Ctrl+C handler for this state.
    pub fn install() -> Arc<Self> {
        let state = Arc::new(InterruptState::default());
        let s = state.clone();
        tokio::spawn(async move {
            while tokio::signal::ctrl_c().await.is_ok() {
                if !s.in_fix.load(Ordering::SeqCst) {
                    eprintln!("\nInterrupted. Run arca fsck --repair again to continue.");
                    std::process::exit(130);
                }
                s.interrupted.store(true, Ordering::SeqCst);
                eprintln!("\nInterrupt received: stopping after the current fix.");
            }
        });
        state
    }
}

/// What happened to one item.
enum Outcome {
    Fixed,
    /// The item changed since the scan and no longer needs (or allows) the fix.
    Changed(String),
    Failed(String),
}

/// What happened to one category.
#[derive(Debug, Default, Clone)]
pub struct CategoryStats {
    pub name: &'static str,
    pub fixed: usize,
    pub declined: usize,
    pub changed: usize,
    pub failed: usize,
}

/// What a repair run did.
#[derive(Debug, Default)]
pub struct RepairSummary {
    pub categories: Vec<CategoryStats>,
    /// Database fixes were applied (the caller rescans before the file phase).
    pub database_changed: bool,
    /// The run was stopped (`q` or Ctrl+C) before visiting every item.
    pub stopped: bool,
    /// Where quarantined files went, if any were moved.
    pub lost_found: Option<PathBuf>,
}

enum Decision {
    Fix,
    Skip,
    Stop,
}

/// Applies the fixes of one phase.
pub struct Repairer<'a> {
    pub metadata: &'a dyn MetadataStore,
    pub fs: &'a FsBlobStore,
    pub blobs_dir: &'a Path,
    /// `<data_dir>/lost+found/<run timestamp>`, created on first use.
    pub lost_found: PathBuf,
    pub opts: &'a RepairOptions,
    pub cutoff: SystemTime,
    pub prompter: &'a mut dyn Prompter,
    pub interrupt: &'a InterruptState,
    pub summary: RepairSummary,
}

impl Repairer<'_> {
    /// Asks (unless `all`), and on `Fix` marks a fix in progress until
    /// [`Self::record`] closes it.
    fn decide(&mut self, all: &mut bool, question: &str) -> Decision {
        if self.summary.stopped || self.interrupt.interrupted.load(Ordering::SeqCst) {
            self.summary.stopped = true;
            return Decision::Stop;
        }
        if !*all {
            match self.prompter.ask(question) {
                Answer::Yes => {}
                Answer::All => *all = true,
                Answer::No => return Decision::Skip,
                Answer::Quit => {
                    self.summary.stopped = true;
                    return Decision::Stop;
                }
            }
        }
        self.interrupt.in_fix.store(true, Ordering::SeqCst);
        Decision::Fix
    }

    fn header(name: &str, count: usize) {
        println!("\n== {name}: {count} item(s) ==");
    }

    /// Counts the outcome of a fix and closes it (see [`Self::decide`]).
    fn record(&self, stats: &mut CategoryStats, label: &str, outcome: Outcome) {
        self.interrupt.in_fix.store(false, Ordering::SeqCst);
        match outcome {
            Outcome::Fixed => stats.fixed += 1,
            Outcome::Changed(why) => {
                stats.changed += 1;
                println!("  skipped {label}: {why}");
            }
            Outcome::Failed(why) => {
                stats.failed += 1;
                println!("  FAILED {label}: {why}");
            }
        }
    }

    /// Database phase: deletes the rows of lost versions and aborts old
    /// unfinished uploads. Not run on a cluster node.
    pub async fn database_phase(&mut self, report: &FsckReport) {
        if self.opts.cluster {
            if !report.missing_blobs.is_empty() {
                println!(
                    "\nMISSING_BLOB: {} item(s) not repaired on a cluster node: anti-entropy fetches a missing blob from a peer that has it.",
                    report.missing_blobs.len()
                );
            }
            if self.opts.abort_uploads_older_than.is_some() && !report.unfinished_uploads.is_empty() {
                println!(
                    "\nUNFINISHED_UPLOAD: not aborted on a cluster node: abort them with AbortMultipartUpload through the S3 API, which replicates."
                );
            }
            return;
        }
        self.missing_blobs(&report.missing_blobs).await;
        if let Some(age) = self.opts.abort_uploads_older_than {
            let cutoff = chrono::Utc::now() - age;
            let old: Vec<&UnfinishedUpload> = report
                .unfinished_uploads
                .iter()
                .filter(|u| u.initiated_at <= cutoff)
                .collect();
            self.unfinished_uploads(&old, cutoff).await;
        }
    }

    /// File phase: stale temp files, orphans, sidecar mismatches.
    pub async fn file_phase(&mut self, report: &FsckReport) {
        self.stale_tmp_files(&report.stale_tmp_files).await;
        self.orphans(report).await;
        self.sidecar_mismatches(&report.sidecar_mismatches).await;
    }

    async fn missing_blobs(&mut self, items: &[MissingBlob]) {
        if items.is_empty() {
            return;
        }
        Self::header("MISSING_BLOB", items.len());
        let mut stats = CategoryStats { name: "missing blobs", ..Default::default() };
        let mut all = self.opts.yes;
        for m in items {
            let label = format!("{}/{} (version {})", m.bucket, m.key, version_label(&m.version_id));
            let question = format!("Delete the row of {label}? Its data is lost; re-upload it afterwards");
            match self.decide(&mut all, &question) {
                Decision::Fix => {}
                Decision::Skip => {
                    stats.declined += 1;
                    continue;
                }
                Decision::Stop => break,
            }
            let outcome = self.fix_missing_blob(m).await;
            if matches!(outcome, Outcome::Fixed) {
                self.summary.database_changed = true;
                println!("  deleted the row of {label}");
            }
            self.record(&mut stats, &label, outcome);
        }
        self.summary.categories.push(stats);
    }

    async fn fix_missing_blob(&self, m: &MissingBlob) -> Outcome {
        let version = version_label(&m.version_id);
        let row = match self.metadata.get_object_version(&m.bucket, &m.key, version).await {
            Ok(Some(row)) => row,
            Ok(None) => return Outcome::Changed("the version no longer exists".into()),
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        if row.blob_id != m.blob_id {
            return Outcome::Changed("the version now points at another blob".into());
        }
        match self.data_is_missing(&row.blob_id).await {
            Ok(true) => {}
            Ok(false) => return Outcome::Changed("its data is on disk again".into()),
            Err(e) => return Outcome::Failed(e),
        }
        let pre = DeletePrecondition {
            if_match_blob_id: Some(m.blob_id.clone()),
            ..Default::default()
        };
        match self.metadata.delete_object_version_if(&m.bucket, &m.key, version, &pre).await {
            Ok(_) => Outcome::Fixed,
            Err(ArcaError::S3(e)) if e.code == S3ErrorCode::PreconditionFailed => {
                Outcome::Changed("the version changed during the fix".into())
            }
            Err(e) => Outcome::Failed(e.to_string()),
        }
    }

    /// True when the blob file, or any part of a composite, is absent.
    async fn data_is_missing(&self, blob_id: &BlobId) -> Result<bool, String> {
        let parts = match RawBlobOps::read_sidecar(self.fs, blob_id).await {
            Ok(Some(meta)) => meta.composite,
            Ok(None) => None,
            Err(e) => return Err(format!("cannot read the sidecar: {e}")),
        };
        let ids: Vec<BlobId> = match parts {
            Some(parts) => parts.into_iter().map(|p| p.blob_id).collect(),
            None => vec![blob_id.clone()],
        };
        for id in &ids {
            if !tokio::fs::try_exists(self.fs.blob_path(id)).await.map_err(|e| e.to_string())? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn unfinished_uploads(&mut self, items: &[&UnfinishedUpload], cutoff: chrono::DateTime<chrono::Utc>) {
        if items.is_empty() {
            return;
        }
        Self::header("UNFINISHED_UPLOAD", items.len());
        let mut stats = CategoryStats { name: "aborted uploads", ..Default::default() };
        let mut all = self.opts.yes;
        for u in items {
            let label = format!("upload {} of {}/{}", u.upload_id, u.bucket, u.key);
            let question = format!(
                "Abort {label} (initiated {}, {} part(s), {})?",
                u.initiated_at.to_rfc3339(),
                u.parts,
                human_bytes(u.bytes)
            );
            match self.decide(&mut all, &question) {
                Decision::Fix => {}
                Decision::Skip => {
                    stats.declined += 1;
                    continue;
                }
                Decision::Stop => break,
            }
            let outcome = match self.metadata.get_multipart_upload(&u.upload_id).await {
                Ok(Some(upload)) if upload.initiated_at <= cutoff => {
                    if crate::worker::abort_stale_upload(self.metadata, self.fs, None, &u.bucket, &upload).await {
                        self.summary.database_changed = true;
                        println!("  aborted {label}");
                        Outcome::Fixed
                    } else {
                        Outcome::Failed("the abort failed (see the log)".into())
                    }
                }
                Ok(Some(_)) => Outcome::Changed("no longer older than the cutoff".into()),
                Ok(None) => Outcome::Changed("the upload was completed or aborted meanwhile".into()),
                Err(e) => Outcome::Failed(e.to_string()),
            };
            self.record(&mut stats, &label, outcome);
        }
        self.summary.categories.push(stats);
    }

    async fn stale_tmp_files(&mut self, items: &[DiskFile]) {
        if items.is_empty() || self.summary.stopped {
            return;
        }
        Self::header("STALE_TMP", items.len());
        let mut stats = CategoryStats { name: "stale temp files", ..Default::default() };
        let mut all = true; // partial bytes nothing reads: never asked
        for f in items {
            if let Decision::Stop = self.decide(&mut all, "") {
                break;
            }
            let label = f.path.display().to_string();
            let outcome = match self.still_old(&f.path).await {
                Ok(true) => match RawBlobOps::delete_temp_file(self.fs, &f.path).await {
                    Ok(()) => Outcome::Fixed,
                    Err(e) => Outcome::Failed(e.to_string()),
                },
                Ok(false) => Outcome::Changed("modified since the scan".into()),
                Err(why) => Outcome::Changed(why),
            };
            self.record(&mut stats, &label, outcome);
        }
        println!("  removed {} stale temp file(s)", stats.fixed);
        self.summary.categories.push(stats);
    }

    /// The file exists and is still older than the grace period.
    async fn still_old(&self, path: &Path) -> Result<bool, String> {
        match tokio::fs::metadata(path).await {
            Ok(md) => Ok(md.modified().unwrap_or(SystemTime::UNIX_EPOCH) <= self.cutoff),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err("already gone".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn orphans(&mut self, report: &FsckReport) {
        if (report.orphaned_blobs.is_empty() && report.orphaned_sidecars.is_empty()) || self.summary.stopped {
            return;
        }
        if !report.unreadable_sidecars.is_empty() {
            println!(
                "\nORPHANED_BLOB / ORPHANED_SIDECAR: not repaired while {} sidecar(s) are unreadable: one may list the parts of a live composite. Fix or remove them first.",
                report.unreadable_sidecars.len()
            );
            return;
        }
        // Recomputed now, not taken from the scan: what is live may have
        // changed, and an orphan fix must never touch a live file.
        let referenced = match referenced_blob_ids(self.metadata, self.fs).await {
            Ok(r) => r,
            Err(e) => {
                println!("\nORPHANED_BLOB / ORPHANED_SIDECAR: not repaired, cannot compute the referenced blobs: {e}");
                return;
            }
        };
        let verb = if self.opts.delete { "Delete" } else { "Move to lost+found" };

        for (name, items, is_blob) in [
            ("ORPHANED_BLOB", &report.orphaned_blobs, true),
            ("ORPHANED_SIDECAR", &report.orphaned_sidecars, false),
        ] {
            if items.is_empty() || self.summary.stopped {
                continue;
            }
            Self::header(name, items.len());
            let mut stats = CategoryStats {
                name: if is_blob { "orphaned blobs" } else { "orphaned sidecars" },
                ..Default::default()
            };
            let mut all = self.opts.yes;
            for (id, f) in items {
                let what = if is_blob { "orphaned blob" } else { "orphaned sidecar" };
                let label = format!("{what} {id}");
                let question = format!("{verb}: {label} ({})?", human_bytes(f.size));
                match self.decide(&mut all, &question) {
                    Decision::Fix => {}
                    Decision::Skip => {
                        stats.declined += 1;
                        continue;
                    }
                    Decision::Stop => break,
                }
                let outcome = if referenced.contains(id) {
                    Outcome::Changed("now referenced".into())
                } else if !is_blob && self.fs.blob_path(id).exists() {
                    Outcome::Changed("its blob file is back".into())
                } else {
                    match self.still_old(&f.path).await {
                        Ok(true) => {
                            // A blob takes its sidecar along.
                            let mut paths = vec![f.path.clone()];
                            if is_blob {
                                paths.push(self.fs.sidecar_path(id));
                            }
                            self.remove_or_quarantine(&paths).await
                        }
                        Ok(false) => Outcome::Changed("modified since the scan".into()),
                        Err(why) => Outcome::Changed(why),
                    }
                };
                self.record(&mut stats, &label, outcome);
            }
            println!("  {} {}", stats.fixed, if self.opts.delete { "deleted" } else { "moved to lost+found" });
            self.summary.categories.push(stats);
        }
    }

    /// Deletes the files, or moves them under `lost+found` keeping their path
    /// relative to the blobs directory. A file that is already gone is fine.
    async fn remove_or_quarantine(&mut self, paths: &[PathBuf]) -> Outcome {
        for path in paths {
            let result = if self.opts.delete {
                match tokio::fs::remove_file(path).await {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    other => other.map_err(|e| e.to_string()),
                }
            } else {
                self.quarantine(path).await
            };
            if let Err(e) = result {
                return Outcome::Failed(e);
            }
        }
        Outcome::Fixed
    }

    async fn quarantine(&mut self, path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let rel = path
            .strip_prefix(self.blobs_dir)
            .map_err(|_| format!("{} is outside the blobs directory", path.display()))?;
        let dest = self.lost_found.join(rel);
        let (src, dest_for_task) = (path.to_path_buf(), dest.clone());
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = dest_for_task.parent() {
                std::fs::create_dir_all(parent)?;
            }
            rename_durable(&src, &dest_for_task)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("cannot move to {}: {e} (use --delete if lost+found is on another filesystem)", dest.display()))?;
        self.summary.lost_found = Some(self.lost_found.clone());
        Ok(())
    }

    async fn sidecar_mismatches(&mut self, items: &[SidecarMismatch]) {
        if items.is_empty() || self.summary.stopped {
            return;
        }
        Self::header("SIDECAR_MISMATCH", items.len());
        let mut stats = CategoryStats { name: "sidecar mismatches", ..Default::default() };
        let mut all = self.opts.yes;
        for m in items {
            let label = format!("sidecar {} of {}/{}", m.blob_id, m.bucket, m.key);
            let question = format!("Rewrite {label} from the database ({})?", m.details);
            match self.decide(&mut all, &question) {
                Decision::Fix => {}
                Decision::Skip => {
                    stats.declined += 1;
                    continue;
                }
                Decision::Stop => break,
            }
            let outcome = self.fix_sidecar(m).await;
            self.record(&mut stats, &label, outcome);
        }
        println!("  {} sidecar(s) rewritten", stats.fixed);
        self.summary.categories.push(stats);
    }

    /// Rewrites the row-owned fields of the sidecar; encryption, compression
    /// and the composite part list are kept as they are.
    async fn fix_sidecar(&self, m: &SidecarMismatch) -> Outcome {
        let row = match self
            .metadata
            .get_object_version(&m.bucket, &m.key, version_label(&m.version_id))
            .await
        {
            Ok(Some(row)) if row.blob_id == m.blob_id => row,
            Ok(_) => return Outcome::Changed("the version no longer points at this blob".into()),
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        let mut meta = match RawBlobOps::read_sidecar(self.fs, &m.blob_id).await {
            Ok(Some(meta)) => meta,
            Ok(None) => return Outcome::Changed("the sidecar is gone".into()),
            Err(e) => return Outcome::Failed(e.to_string()),
        };
        if sidecar_diff(&meta, &row).is_none() {
            return Outcome::Changed("already consistent".into());
        }
        meta.bucket = row.bucket.clone();
        meta.key = row.key.clone();
        meta.size = row.size;
        meta.etag = row.etag.clone();
        meta.version_id = row.version_id.clone();
        match RawBlobOps::write_sidecar(self.fs, &m.blob_id, &meta).await {
            Ok(()) => Outcome::Fixed,
            Err(e) => Outcome::Failed(e.to_string()),
        }
    }
}

/// Prints what a repair run did.
pub fn print_summary(summary: &RepairSummary) {
    if summary.categories.is_empty() {
        println!("\nNothing was repaired.");
    } else {
        println!("\nRepair summary:");
        for c in &summary.categories {
            println!(
                "  {:<22}{:>8} fixed, {} declined, {} changed since the scan, {} failed",
                c.name, c.fixed, c.declined, c.changed, c.failed
            );
        }
    }
    if let Some(dir) = &summary.lost_found {
        println!(
            "Orphaned files were moved to {}: delete that directory once you are satisfied.",
            dir.display()
        );
    }
    if summary.stopped {
        println!("Stopped before the end: run arca fsck --repair again to continue.");
    }
}
