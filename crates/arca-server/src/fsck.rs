//! Filesystem consistency check: compare the metadata with the files on disk.
//!
//! What counts as referenced is the same set blob GC computes (live object
//! rows, in-progress multipart parts, and the parts of every live composite,
//! via [`crate::blob_gc::add_live_composite_parts`]), so `arca fsck` and
//! `arca gc` never disagree on what is live. A composite object has no blob
//! file of its own: its sidecar lists the part blobs, and the object is
//! missing data only when one of those parts is.
//!
//! Files younger than the grace period are skipped rather than reported: on a
//! running server they may belong to a write that has not committed its row
//! yet.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use tokio::fs;
use tokio::io::AsyncReadExt;

use arca_core::store::blob::SidecarMeta;
use arca_core::store::MetadataStore;
use arca_core::types::{BlobId, ObjectRecord};
use arca_storage::fs::{temp_file_kind, TempFileKind};

use crate::blob_gc::add_live_composite_parts;
use crate::config::Config;

mod repair;

pub use repair::RepairOptions;
use repair::{Prompter, Repairer};

/// Page size for listing multipart uploads.
const UPLOAD_PAGE: u32 = 1000;

/// Options of an `arca fsck` run.
#[derive(Debug, Clone)]
pub struct FsckOptions {
    /// Verify blob checksums against stored ETags (reads every blob file).
    pub verify_checksums: bool,
    /// Files modified within this window are skipped (may be in flight).
    pub grace: Duration,
}

/// A file found in the blobs directory.
#[derive(Debug, Clone)]
pub struct DiskFile {
    pub path: PathBuf,
    pub size: u64,
    pub modified: SystemTime,
}

/// A live object version whose data is not on disk.
#[derive(Debug)]
pub struct MissingBlob {
    pub bucket: String,
    pub key: String,
    pub version_id: Option<String>,
    pub blob_id: BlobId,
    /// For a composite object, the absent part blobs (out of `total_parts`);
    /// empty for a single-blob object, whose blob file itself is absent.
    pub missing_parts: Vec<BlobId>,
    pub total_parts: usize,
}

/// A sidecar whose fields disagree with the object row.
#[derive(Debug)]
pub struct SidecarMismatch {
    pub blob_id: BlobId,
    pub bucket: String,
    pub key: String,
    pub version_id: Option<String>,
    pub details: String,
}

/// A sidecar that cannot be read or parsed.
#[derive(Debug)]
pub struct UnreadableSidecar {
    pub blob_id: BlobId,
    pub path: PathBuf,
    pub error: String,
}

/// A blob whose MD5 differs from the stored ETag.
#[derive(Debug)]
pub struct ChecksumError {
    pub blob_id: BlobId,
    pub expected: String,
    pub actual: String,
}

/// A multipart upload that was never completed nor aborted.
#[derive(Debug)]
pub struct UnfinishedUpload {
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
    pub initiated_at: chrono::DateTime<chrono::Utc>,
    pub parts: usize,
    pub bytes: u64,
}

/// What a scan looked at.
#[derive(Debug, Default)]
pub struct ScanTotals {
    pub blobs: usize,
    pub sidecars: usize,
    pub temp_files: usize,
    pub versions: usize,
    pub delete_markers: usize,
}

/// Result of a scan.
#[derive(Debug, Default)]
pub struct FsckReport {
    pub totals: ScanTotals,
    pub orphaned_blobs: Vec<(BlobId, DiskFile)>,
    pub orphaned_sidecars: Vec<(BlobId, DiskFile)>,
    pub missing_blobs: Vec<MissingBlob>,
    pub sidecar_mismatches: Vec<SidecarMismatch>,
    pub unreadable_sidecars: Vec<UnreadableSidecar>,
    pub stale_tmp_files: Vec<DiskFile>,
    pub rewrite_recovery_files: Vec<DiskFile>,
    pub checksum_errors: Vec<ChecksumError>,
    /// Informational: files under `blobs/` that Arca did not write (the name
    /// is not a blob id), so they are neither blobs nor repaired. Listed
    /// because they take space `du` shows but no Arca figure explains.
    pub foreign_files: Vec<DiskFile>,
    /// Informational: valid state, not an inconsistency.
    pub unfinished_uploads: Vec<UnfinishedUpload>,
    /// Files skipped because they are younger than the grace period.
    pub recent_skipped: usize,
}

impl FsckReport {
    /// Number of inconsistencies (unfinished uploads are not one).
    pub fn issue_count(&self) -> usize {
        self.orphaned_blobs.len()
            + self.orphaned_sidecars.len()
            + self.missing_blobs.len()
            + self.sidecar_mismatches.len()
            + self.unreadable_sidecars.len()
            + self.stale_tmp_files.len()
            + self.rewrite_recovery_files.len()
            + self.checksum_errors.len()
    }
}

/// The grace period to use: on an HA cluster node at least the tombstone
/// grace, the window anti-entropy GC also waits, because a blob can arrive
/// before its row does and must outlive the replication lag.
pub fn effective_grace(requested: Duration, cluster: Option<&crate::config::ClusterConfig>) -> Duration {
    match cluster.filter(|c| c.enabled) {
        Some(c) => requested.max(c.tombstone_grace()),
        None => requested,
    }
}

/// Runs the `arca fsck` command, repairing when `repair` is given. Returns the
/// exit code: 0 when no issue is (left), 1 otherwise.
pub async fn run_fsck(config: &Config, opts: &FsckOptions, repair: Option<&RepairOptions>) -> Result<i32> {
    use std::io::IsTerminal as _;
    match repair {
        None => check(config, opts, None).await,
        Some(r) => {
            if !r.yes && !std::io::stdin().is_terminal() {
                anyhow::bail!(
                    "--repair asks before each fix and needs a terminal: pass --yes to answer yes to every question"
                );
            }
            let interrupt = repair::InterruptState::install();
            let mut prompter = repair::TtyPrompter;
            check(config, opts, Some((r, &mut prompter, interrupt.as_ref()))).await
        }
    }
}

type RepairCtx<'a> = (&'a RepairOptions, &'a mut dyn Prompter, &'a repair::InterruptState);

async fn check(config: &Config, opts: &FsckOptions, repair: Option<RepairCtx<'_>>) -> Result<i32> {
    let blobs_dir = config.storage.blobs_dir();
    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }
    let data_dir = Path::new(&config.storage.data_dir);
    if repair.is_some() {
        crate::rewrite_marker::ensure_none(data_dir, "repair")?;
    }

    println!("Opening database...");
    let metadata = crate::offline_store::open_metadata_store(config).await?;

    println!("Scanning {}...", blobs_dir.display());
    let mut report = scan(metadata.as_ref(), &blobs_dir, opts).await?;
    print_report(&report, opts);

    let Some((ropts, prompter, interrupt)) = repair else {
        return Ok(exit_code(report.issue_count()));
    };

    let fs = arca_storage::FsBlobStore::new(&blobs_dir, config.storage.blob_prefix_depth)
        .await
        .context("opening blob store")?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let mut repairer = Repairer {
        metadata: metadata.as_ref(),
        fs: &fs,
        blobs_dir: &blobs_dir,
        lost_found: data_dir.join("lost+found").join(stamp),
        opts: ropts,
        cutoff: SystemTime::now().checked_sub(opts.grace).unwrap_or(SystemTime::UNIX_EPOCH),
        prompter,
        interrupt,
        summary: Default::default(),
    };

    repairer.database_phase(&report).await;
    if repairer.summary.database_changed {
        // Rows are gone: their leftover files are orphans now, found here so
        // this same run can reclaim them.
        println!("\nRescanning after the database repairs...");
        report = scan(metadata.as_ref(), &blobs_dir, opts).await?;
        print_report(&report, opts);
    }
    repairer.file_phase(&report).await;
    repair::print_summary(&repairer.summary);

    // What is left, exactly: an item skipped because it changed since the
    // scan may have stopped being an issue. Checksums are not re-verified (no
    // repair touches them): the scan's errors still stand.
    let rescan_opts = FsckOptions { verify_checksums: false, ..opts.clone() };
    let left = scan(metadata.as_ref(), &blobs_dir, &rescan_opts).await?;
    let remaining = left.issue_count() + report.checksum_errors.len();
    if remaining == 0 {
        println!("\nNo issues left.");
        Ok(0)
    } else {
        println!("\n{remaining} issue(s) left: run arca fsck to see them.");
        Ok(1)
    }
}

fn exit_code(issues: usize) -> i32 {
    if issues == 0 {
        println!("No issues found.");
        0
    } else {
        println!("\n{issues} issue(s) found.");
        1
    }
}

/// Compares the metadata with the blobs directory. Read-only.
pub async fn scan(
    metadata: &dyn MetadataStore,
    blobs_dir: &Path,
    opts: &FsckOptions,
) -> Result<FsckReport> {
    let inv = Inventory::walk(blobs_dir).await?;
    let (sidecars, unreadable_sidecars) = read_sidecars(&inv.sidecars).await;

    let buckets = metadata.list_buckets().await.context("listing buckets")?;
    let mut rows = Vec::new();
    for b in &buckets {
        rows.extend(
            metadata
                .list_all_object_versions(&b.name, None)
                .await
                .with_context(|| format!("listing object versions of {}", b.name))?,
        );
    }
    let live: Vec<&ObjectRecord> = rows
        .iter()
        .filter(|r| !r.is_delete_marker && !r.is_tombstone && !r.blob_id.0.is_empty())
        .collect();

    let mut referenced: HashSet<BlobId> = metadata
        .list_referenced_blob_ids()
        .await
        .context("listing referenced blobs")?
        .into_iter()
        .collect();
    add_live_composite_parts(&mut referenced, sidecars.iter());

    let cutoff = SystemTime::now()
        .checked_sub(opts.grace)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut report = FsckReport {
        totals: ScanTotals {
            blobs: inv.blobs.len(),
            sidecars: inv.sidecars.len(),
            temp_files: inv.reclaimable_tmp.len() + inv.recovery_tmp.len(),
            versions: rows.len(),
            delete_markers: rows.iter().filter(|r| r.is_delete_marker).count(),
        },
        unreadable_sidecars,
        ..Default::default()
    };
    let unreadable: HashSet<&BlobId> = report.unreadable_sidecars.iter().map(|u| &u.blob_id).collect();

    for row in &live {
        match sidecars.get(&row.blob_id) {
            Some(meta) => {
                if let Some(parts) = &meta.composite {
                    let missing: Vec<BlobId> = parts
                        .iter()
                        .filter(|p| !inv.blobs.contains_key(&p.blob_id))
                        .map(|p| p.blob_id.clone())
                        .collect();
                    if !missing.is_empty() {
                        report.missing_blobs.push(missing_blob(row, missing, parts.len()));
                    }
                } else if !inv.blobs.contains_key(&row.blob_id) {
                    report.missing_blobs.push(missing_blob(row, Vec::new(), 0));
                }
                if let Some(details) = sidecar_diff(meta, row) {
                    report.sidecar_mismatches.push(SidecarMismatch {
                        blob_id: row.blob_id.clone(),
                        bucket: row.bucket.clone(),
                        key: row.key.clone(),
                        version_id: row.version_id.clone(),
                        details,
                    });
                }
            }
            // An unreadable sidecar may describe a composite: whether data is
            // missing is unknown, and it is already reported as unreadable.
            None if unreadable.contains(&row.blob_id) => {}
            None => {
                if !inv.blobs.contains_key(&row.blob_id) {
                    report.missing_blobs.push(missing_blob(row, Vec::new(), 0));
                }
            }
        }
    }

    for (id, file) in &inv.blobs {
        if referenced.contains(id) {
            continue;
        }
        if file.modified <= cutoff {
            report.orphaned_blobs.push((id.clone(), file.clone()));
        } else {
            report.recent_skipped += 1;
        }
    }

    // A sidecar next to an orphaned blob goes with that blob; one with neither
    // a blob nor a row (e.g. the sidecar of an orphaned composite) is orphaned.
    for (id, file) in &inv.sidecars {
        if referenced.contains(id) || inv.blobs.contains_key(id) || unreadable.contains(id) {
            continue;
        }
        if file.modified <= cutoff {
            report.orphaned_sidecars.push((id.clone(), file.clone()));
        } else {
            report.recent_skipped += 1;
        }
    }

    for file in &inv.reclaimable_tmp {
        if file.modified <= cutoff {
            report.stale_tmp_files.push(file.clone());
        } else {
            report.recent_skipped += 1;
        }
    }
    report.rewrite_recovery_files = inv.recovery_tmp.clone();
    report.foreign_files = inv.foreign.clone();

    if opts.verify_checksums {
        report.checksum_errors = check_checksums(&live, &sidecars, &inv.blobs).await;
    }

    for b in &buckets {
        report
            .unfinished_uploads
            .extend(list_unfinished_uploads(metadata, &b.name).await?);
    }

    sort_report(&mut report);
    Ok(report)
}

fn missing_blob(row: &ObjectRecord, missing_parts: Vec<BlobId>, total_parts: usize) -> MissingBlob {
    MissingBlob {
        bucket: row.bucket.clone(),
        key: row.key.clone(),
        version_id: row.version_id.clone(),
        blob_id: row.blob_id.clone(),
        missing_parts,
        total_parts,
    }
}

/// The fields of `meta` that disagree with `row`, if any. Sidecars written
/// before the row's version id was minted carry none, which is not a mismatch.
fn sidecar_diff(meta: &SidecarMeta, row: &ObjectRecord) -> Option<String> {
    let mut diffs = Vec::new();
    if meta.bucket != row.bucket {
        diffs.push(format!("bucket: sidecar={}, db={}", meta.bucket, row.bucket));
    }
    if meta.key != row.key {
        diffs.push(format!("key: sidecar={}, db={}", meta.key, row.key));
    }
    if meta.size != row.size {
        diffs.push(format!("size: sidecar={}, db={}", meta.size, row.size));
    }
    if meta.etag != row.etag {
        diffs.push(format!("etag: sidecar={}, db={}", meta.etag, row.etag));
    }
    if meta.version_id.is_some() && meta.version_id != row.version_id {
        diffs.push(format!(
            "version_id: sidecar={}, db={}",
            meta.version_id.as_deref().unwrap_or("null"),
            row.version_id.as_deref().unwrap_or("null")
        ));
    }
    (!diffs.is_empty()).then(|| diffs.join("; "))
}

/// Every in-progress multipart upload of `bucket`, with its part count and size.
pub(crate) async fn list_unfinished_uploads(
    metadata: &dyn MetadataStore,
    bucket: &str,
) -> Result<Vec<UnfinishedUpload>> {
    let mut out = Vec::new();
    let mut key_marker: Option<String> = None;
    let mut upload_id_marker: Option<String> = None;
    loop {
        let page = metadata
            .list_multipart_uploads(
                bucket,
                None,
                key_marker.as_deref(),
                upload_id_marker.as_deref(),
                UPLOAD_PAGE,
            )
            .await
            .with_context(|| format!("listing multipart uploads of {bucket}"))?;
        let full = page.len() as u32 == UPLOAD_PAGE;
        if let Some(last) = page.last() {
            key_marker = Some(last.key.clone());
            upload_id_marker = Some(last.upload_id.clone());
        }
        for upload in page {
            let parts = metadata
                .list_parts(&upload.upload_id)
                .await
                .with_context(|| format!("listing parts of upload {}", upload.upload_id))?;
            out.push(UnfinishedUpload {
                bucket: upload.bucket,
                key: upload.key,
                upload_id: upload.upload_id,
                initiated_at: upload.initiated_at,
                parts: parts.len(),
                bytes: parts.iter().map(|p| p.size).sum(),
            });
        }
        if !full {
            return Ok(out);
        }
    }
}

/// Deterministic output order.
fn sort_report(r: &mut FsckReport) {
    r.orphaned_blobs.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    r.orphaned_sidecars.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    r.missing_blobs
        .sort_by(|a, b| (&a.bucket, &a.key, &a.version_id).cmp(&(&b.bucket, &b.key, &b.version_id)));
    r.sidecar_mismatches.sort_by(|a, b| a.blob_id.0.cmp(&b.blob_id.0));
    r.unreadable_sidecars.sort_by(|a, b| a.path.cmp(&b.path));
    r.stale_tmp_files.sort_by(|a, b| a.path.cmp(&b.path));
    r.rewrite_recovery_files.sort_by(|a, b| a.path.cmp(&b.path));
    r.checksum_errors.sort_by(|a, b| a.blob_id.0.cmp(&b.blob_id.0));
    r.foreign_files.sort_by(|a, b| a.path.cmp(&b.path));
    r.unfinished_uploads.sort_by_key(|u| u.initiated_at);
}

/// The files under `blobs/` that Arca did not write, as `arca fsck` sees them.
pub(crate) async fn find_foreign_files(blobs_dir: &Path) -> Result<Vec<DiskFile>> {
    Ok(Inventory::walk(blobs_dir).await?.foreign)
}

/// The files of the blobs directory, by kind.
#[derive(Default)]
struct Inventory {
    blobs: HashMap<BlobId, DiskFile>,
    sidecars: HashMap<BlobId, DiskFile>,
    reclaimable_tmp: Vec<DiskFile>,
    recovery_tmp: Vec<DiskFile>,
    /// Files whose name is not a blob id (nor a temp file name).
    foreign: Vec<DiskFile>,
}

impl Inventory {
    async fn walk(blobs_dir: &Path) -> Result<Self> {
        let mut inv = Inventory::default();
        let mut stack = vec![blobs_dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut read_dir = fs::read_dir(&dir)
                .await
                .with_context(|| format!("reading directory: {}", dir.display()))?;
            while let Some(entry) = read_dir.next_entry().await? {
                let path = entry.path();
                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
                    continue;
                };
                let md = entry
                    .metadata()
                    .await
                    .with_context(|| format!("reading metadata of {}", path.display()))?;
                let file = DiskFile {
                    path,
                    size: md.len(),
                    modified: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                };
                match temp_file_kind(&name) {
                    Some(TempFileKind::Reclaimable) => inv.reclaimable_tmp.push(file),
                    Some(TempFileKind::RewriteRecovery) => inv.recovery_tmp.push(file),
                    None => {
                        let (id, is_sidecar) = match name.strip_suffix(".meta") {
                            Some(id) => (BlobId(id.to_string()), true),
                            // Anything else is a blob: its file name is its id.
                            None => (BlobId(name), false),
                        };
                        if !id.is_valid() {
                            // Not written by Arca (TD-041: never treat it as a blob).
                            inv.foreign.push(file);
                        } else if is_sidecar {
                            inv.sidecars.insert(id, file);
                        } else {
                            inv.blobs.insert(id, file);
                        }
                    }
                }
            }
        }
        Ok(inv)
    }
}

/// Parses every sidecar; the ones that cannot be read are returned apart.
async fn read_sidecars(
    files: &HashMap<BlobId, DiskFile>,
) -> (HashMap<BlobId, SidecarMeta>, Vec<UnreadableSidecar>) {
    let mut parsed = HashMap::new();
    let mut unreadable = Vec::new();
    for (id, file) in files {
        let result = match fs::read_to_string(&file.path).await {
            Ok(json) => serde_json::from_str::<SidecarMeta>(&json).map_err(|e| format!("malformed sidecar JSON: {e}")),
            Err(e) => Err(format!("could not read sidecar: {e}")),
        };
        match result {
            Ok(meta) => {
                parsed.insert(id.clone(), meta);
            }
            Err(error) => unreadable.push(UnreadableSidecar {
                blob_id: id.clone(),
                path: file.path.clone(),
                error,
            }),
        }
    }
    (parsed, unreadable)
}

/// Verifies the MD5 of every single-blob object stored as plain bytes.
/// Multipart (composite ETag), encrypted and compressed objects are skipped:
/// their on-disk bytes are not what the ETag hashes.
async fn check_checksums(
    live: &[&ObjectRecord],
    sidecars: &HashMap<BlobId, SidecarMeta>,
    blobs: &HashMap<BlobId, DiskFile>,
) -> Vec<ChecksumError> {
    let mut errors = Vec::new();
    for obj in live {
        if obj.etag.contains('-') || obj.encryption_algorithm.is_some() {
            continue;
        }
        if let Some(meta) = sidecars.get(&obj.blob_id) {
            if meta.compression.is_some() || meta.encryption.is_some() {
                continue;
            }
        }
        let Some(file) = blobs.get(&obj.blob_id) else {
            continue; // reported as missing
        };
        match compute_md5(&file.path).await {
            Ok(actual) if actual != obj.etag => errors.push(ChecksumError {
                blob_id: obj.blob_id.clone(),
                expected: obj.etag.clone(),
                actual,
            }),
            Ok(_) => {}
            Err(e) => eprintln!("WARNING: could not read blob {} for checksum: {e}", obj.blob_id),
        }
    }
    errors
}

/// Computes MD5 hex of a file.
async fn compute_md5(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .await
        .context("opening file for checksum")?;
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.context("reading file")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Formats a byte count with a binary unit (`9.0 GiB`).
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn version_label(version_id: &Option<String>) -> &str {
    version_id.as_deref().unwrap_or("null")
}

fn print_report(report: &FsckReport, opts: &FsckOptions) {
    let t = &report.totals;
    println!(
        "Found {} blob(s), {} sidecar(s), {} temp file(s) on disk",
        t.blobs, t.sidecars, t.temp_files
    );
    println!(
        "Found {} object version(s) in database ({} delete markers)",
        t.versions, t.delete_markers
    );
    println!();
    for (id, f) in &report.orphaned_blobs {
        println!("ORPHANED_BLOB  {id}  {}  referenced by no object, upload or composite", human_bytes(f.size));
    }
    for (id, _) in &report.orphaned_sidecars {
        println!("ORPHANED_SIDECAR  {id}  .meta file with no blob file and no object");
    }
    for m in &report.missing_blobs {
        let what = format!("{}/{} (version {})", m.bucket, m.key, version_label(&m.version_id));
        if m.missing_parts.is_empty() {
            println!("MISSING_BLOB   {}  object {what} references a missing blob file", m.blob_id);
        } else {
            let ids: Vec<&str> = m.missing_parts.iter().map(|p| p.0.as_str()).collect();
            println!(
                "MISSING_BLOB   {}  composite object {what} is missing {} of {} part(s): {}",
                m.blob_id,
                m.missing_parts.len(),
                m.total_parts,
                ids.join(", ")
            );
        }
    }
    for m in &report.sidecar_mismatches {
        println!(
            "SIDECAR_MISMATCH  {}  object {}/{} (version {}): {}",
            m.blob_id,
            m.bucket,
            m.key,
            version_label(&m.version_id),
            m.details
        );
    }
    for u in &report.unreadable_sidecars {
        println!("UNREADABLE_SIDECAR  {}  {}", u.path.display(), u.error);
    }
    for f in &report.stale_tmp_files {
        println!("STALE_TMP  {}  {}  leftover temporary file", f.path.display(), human_bytes(f.size));
    }
    for f in &report.rewrite_recovery_files {
        println!(
            "REWRITE_RECOVERY  {}  unfinished in-place rewrite: re-run the same command to complete it",
            f.path.display()
        );
    }
    for e in &report.checksum_errors {
        println!("CORRUPT  {}  expected={}, actual={}", e.blob_id, e.expected, e.actual);
    }
    for f in &report.foreign_files {
        println!(
            "FOREIGN_FILE  {}  {}  not a file Arca writes (informational: never repaired; `arca gc --reclaim --delete-foreign-files` removes it)",
            f.path.display(),
            human_bytes(f.size)
        );
    }
    for u in &report.unfinished_uploads {
        println!(
            "UNFINISHED_UPLOAD  {}/{}  upload {} initiated {}, {} part(s), {}",
            u.bucket,
            u.key,
            u.upload_id,
            u.initiated_at.to_rfc3339(),
            u.parts,
            human_bytes(u.bytes)
        );
    }

    println!();
    println!("Summary:");
    let line = |label: &str, count: usize, bytes: Option<u64>| {
        let size = bytes.map(|b| format!("  ({})", human_bytes(b))).unwrap_or_default();
        println!("  {label:<22}{count:>8}{size}");
    };
    let sum = |files: &mut dyn Iterator<Item = &DiskFile>| files.map(|f| f.size).sum::<u64>();
    line("orphaned blobs", report.orphaned_blobs.len(), Some(sum(&mut report.orphaned_blobs.iter().map(|(_, f)| f))));
    line("orphaned sidecars", report.orphaned_sidecars.len(), None);
    line("missing blobs", report.missing_blobs.len(), None);
    line("sidecar mismatches", report.sidecar_mismatches.len(), None);
    line("unreadable sidecars", report.unreadable_sidecars.len(), None);
    line("stale temp files", report.stale_tmp_files.len(), Some(sum(&mut report.stale_tmp_files.iter())));
    line("rewrite recovery files", report.rewrite_recovery_files.len(), None);
    if opts.verify_checksums {
        line("checksum errors", report.checksum_errors.len(), None);
    }
    line("foreign files", report.foreign_files.len(), Some(sum(&mut report.foreign_files.iter())));
    let upload_bytes = report.unfinished_uploads.iter().map(|u| u.bytes).sum();
    line("unfinished uploads", report.unfinished_uploads.len(), Some(upload_bytes));
    if let Some(oldest) = report.unfinished_uploads.first() {
        println!(
            "    oldest initiated {} (informational: abort them, or add an AbortIncompleteMultipartUpload lifecycle rule)",
            oldest.initiated_at.to_rfc3339()
        );
    }
    if report.recent_skipped > 0 {
        println!(
            "  {} file(s) younger than the grace period ({}s) were skipped",
            report.recent_skipped,
            opts.grace.as_secs()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::blob::{BlobCompressionInfo, CompositePart, CompressionAlgorithm};
    use arca_core::store::RawBlobOps;
    use arca_core::types::{MultipartUploadRecord, PartRecord};

    const ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ID2: &str = "6ba7b810-9dad-11d1-80b4-00c04fd430c8";
    const PART1: &str = "11111111-1111-4111-8111-111111111111";
    const PART2: &str = "22222222-2222-4222-8222-222222222222";

    fn opts() -> FsckOptions {
        FsckOptions { verify_checksums: false, grace: Duration::ZERO }
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        config: Config,
        store: arca_storage::SqliteStore,
        fs: arca_storage::FsBlobStore,
    }

    impl Fixture {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let config: Config = toml::from_str(&format!(
                "[server]\nbind = \"127.0.0.1\"\nport = 9000\n\n[storage]\ndata_dir = \"{}\"\n",
                tmp.path().display()
            ))
            .unwrap();
            let blobs_dir = config.storage.blobs_dir();
            fs::create_dir_all(&blobs_dir).await.unwrap();
            let store = arca_storage::SqliteStore::open(&config.storage.db_path()).await.unwrap();
            store.create_bucket("bucket").await.unwrap();
            let fs = arca_storage::FsBlobStore::new(&blobs_dir, config.storage.blob_prefix_depth)
                .await
                .unwrap();
            Fixture { _tmp: tmp, config, store, fs }
        }

        async fn scan(&self, opts: &FsckOptions) -> FsckReport {
            scan(&self.store, &self.config.storage.blobs_dir(), opts).await.unwrap()
        }

        fn blob_path(&self, id: &str) -> PathBuf {
            self.fs.blob_path(&BlobId(id.into()))
        }

        async fn write_blob(&self, id: &str, content: &[u8]) {
            let path = self.blob_path(id);
            fs::create_dir_all(path.parent().unwrap()).await.unwrap();
            fs::write(path, content).await.unwrap();
        }

        async fn write_sidecar(&self, id: &str, meta: &SidecarMeta) {
            let path = self.fs.sidecar_path(&BlobId(id.into()));
            fs::create_dir_all(path.parent().unwrap()).await.unwrap();
            fs::write(path, serde_json::to_string_pretty(meta).unwrap()).await.unwrap();
        }

        async fn write_file_next_to(&self, id: &str, name: &str) {
            let path = self.blob_path(id).with_file_name(name);
            fs::create_dir_all(path.parent().unwrap()).await.unwrap();
            fs::write(path, b"partial").await.unwrap();
        }

        /// Blob + sidecar + row for a plain object.
        async fn put_object(&self, id: &str, key: &str, content: &[u8]) -> ObjectRecord {
            self.write_blob(id, content).await;
            self.write_sidecar(id, &sidecar(key, content.len() as u64, &md5_hex(content))).await;
            let r = record(id, key, content.len() as u64, &md5_hex(content));
            self.store.put_object(&r).await.unwrap();
            r
        }

        /// Two part blobs (with part sidecars) + composite sidecar + row.
        async fn put_composite(&self, id: &str, key: &str) {
            let mut parts = Vec::new();
            for (i, part) in [PART1, PART2].iter().enumerate() {
                self.write_blob(part, b"part-bytes").await;
                self.write_sidecar(part, &sidecar(&format!("{key}#upload#{}", i + 1), 10, &md5_hex(b"part-bytes"))).await;
                parts.push(CompositePart {
                    blob_id: BlobId(part.to_string()),
                    plaintext_size: 10,
                    plaintext_etag: md5_hex(b"part-bytes"),
                    encryption: None,
                });
            }
            let etag = "d41d8cd98f00b204e9800998ecf8427e-2";
            let mut meta = sidecar(key, 20, etag);
            meta.composite = Some(parts);
            self.write_sidecar(id, &meta).await;
            self.store.put_object(&record(id, key, 20, etag)).await.unwrap();
        }
    }

    fn md5_hex(data: &[u8]) -> String {
        hex::encode(Md5::digest(data))
    }

    fn sidecar(key: &str, size: u64, etag: &str) -> SidecarMeta {
        SidecarMeta {
            bucket: "bucket".into(),
            key: key.into(),
            size,
            etag: etag.into(),
            content_type: None,
            last_modified: "2024-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    fn record(id: &str, key: &str, size: u64, etag: &str) -> ObjectRecord {
        ObjectRecord {
            bucket: "bucket".into(),
            key: key.into(),
            blob_id: BlobId(id.into()),
            size,
            etag: etag.into(),
            content_type: None,
            last_modified: chrono::Utc::now(),
            metadata: HashMap::new(),
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

    #[tokio::test]
    async fn clean_state_has_no_issues() {
        let f = Fixture::new().await;
        f.put_object(ID, "key.txt", b"hello").await;

        assert_eq!(f.scan(&opts()).await.issue_count(), 0);
        assert_eq!(run_fsck(&f.config, &opts(), None).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn orphaned_blob_is_reported_with_its_size() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan data").await;

        let r = f.scan(&opts()).await;
        assert_eq!(r.orphaned_blobs.len(), 1);
        assert_eq!(r.orphaned_blobs[0].0 .0, ID);
        assert_eq!(r.orphaned_blobs[0].1.size, 11);
        assert_eq!(r.issue_count(), 1);
        assert_eq!(run_fsck(&f.config, &opts(), None).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn files_younger_than_the_grace_period_are_skipped() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"in flight").await;
        f.write_file_next_to(ID2, &format!("{ID2}.tmp")).await;

        let r = f.scan(&FsckOptions { verify_checksums: false, grace: Duration::from_secs(3600) }).await;
        assert_eq!(r.issue_count(), 0);
        assert_eq!(r.recent_skipped, 2);
    }

    #[tokio::test]
    async fn missing_blob_is_reported_with_its_version() {
        let f = Fixture::new().await;
        f.store.put_object(&record(ID, "missing.txt", 10, "abc123")).await.unwrap();

        let r = f.scan(&opts()).await;
        assert_eq!(r.missing_blobs.len(), 1);
        let m = &r.missing_blobs[0];
        assert_eq!((m.key.as_str(), m.blob_id.0.as_str()), ("missing.txt", ID));
        assert!(m.missing_parts.is_empty());
    }

    #[tokio::test]
    async fn sidecar_mismatch_is_reported() {
        let f = Fixture::new().await;
        f.put_object(ID, "key.txt", b"hello").await;
        let mut bad = sidecar("key.txt", 5, &md5_hex(b"hello"));
        bad.bucket = "wrong-bucket".into();
        f.write_sidecar(ID, &bad).await;

        let r = f.scan(&opts()).await;
        assert_eq!(r.sidecar_mismatches.len(), 1);
        assert!(r.sidecar_mismatches[0].details.contains("bucket: sidecar=wrong-bucket"));
    }

    #[tokio::test]
    async fn sidecar_without_version_id_is_not_a_mismatch() {
        let f = Fixture::new().await;
        f.store.set_bucket_config("bucket", "versioning", "Enabled").await.unwrap();
        f.put_object(ID, "key.txt", b"hello").await; // the row gets a version id, the sidecar none

        assert_eq!(f.scan(&opts()).await.issue_count(), 0);
    }

    #[tokio::test]
    async fn unreadable_sidecar_is_reported_and_its_row_is_not_called_missing() {
        let f = Fixture::new().await;
        f.store.put_object(&record(ID, "k", 20, "d41d8cd98f00b204e9800998ecf8427e-2")).await.unwrap();
        let path = f.fs.sidecar_path(&BlobId(ID.into()));
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(&path, b"{ not json").await.unwrap();

        let r = f.scan(&opts()).await;
        assert_eq!(r.unreadable_sidecars.len(), 1);
        assert!(r.missing_blobs.is_empty(), "it may be a composite: {:?}", r.missing_blobs);
    }

    #[tokio::test]
    async fn temp_files_are_classified() {
        let f = Fixture::new().await;
        f.write_file_next_to(ID, &format!("{ID}.tmp")).await;
        f.write_file_next_to(ID, &format!("{ID}.meta.0123456789abcdef0123456789abcdef.tmp")).await;
        f.write_file_next_to(ID2, &format!("{ID2}.rewrite.tmp")).await;
        f.write_file_next_to(ID2, &format!("{ID2}.meta.pending.tmp")).await;

        let r = f.scan(&opts()).await;
        assert_eq!(r.stale_tmp_files.len(), 2);
        assert_eq!(r.rewrite_recovery_files.len(), 2);
        assert!(r.orphaned_blobs.is_empty() && r.orphaned_sidecars.is_empty());
    }

    #[tokio::test]
    async fn rewrite_recovery_files_are_reported_whatever_their_age() {
        let f = Fixture::new().await;
        f.write_file_next_to(ID, &format!("{ID}.rewrite.tmp")).await;

        let r = f.scan(&FsckOptions { verify_checksums: false, grace: Duration::from_secs(3600) }).await;
        assert_eq!(r.rewrite_recovery_files.len(), 1);
    }

    #[tokio::test]
    async fn composite_object_and_its_parts_are_consistent() {
        // The 2026-10-14 incident shape: fsck used to report the composite as
        // MISSING_BLOB + ORPHANED_SIDECAR and each part as ORPHANED_BLOB.
        let f = Fixture::new().await;
        f.put_composite(ID, "large.bin").await;

        let r = f.scan(&opts()).await;
        assert_eq!(r.issue_count(), 0, "{r:?}");
    }

    #[tokio::test]
    async fn composite_with_a_missing_part_is_a_missing_blob() {
        let f = Fixture::new().await;
        f.put_composite(ID, "large.bin").await;
        fs::remove_file(f.blob_path(PART2)).await.unwrap();

        let r = f.scan(&opts()).await;
        assert_eq!(r.missing_blobs.len(), 1);
        let m = &r.missing_blobs[0];
        assert_eq!(m.blob_id.0, ID);
        assert_eq!(m.missing_parts, vec![BlobId(PART2.into())]);
        assert_eq!(m.total_parts, 2);
        // The orphaned part sidecar of the missing part goes unreported: it
        // still belongs to the composite until the row is gone.
        assert_eq!(r.issue_count(), 1, "{r:?}");
    }

    #[tokio::test]
    async fn orphaned_composite_reports_its_sidecar_and_its_parts() {
        let f = Fixture::new().await;
        f.put_composite(ID, "large.bin").await;
        f.store.delete_object("bucket", "large.bin").await.unwrap();

        let r = f.scan(&opts()).await;
        let sidecars: Vec<&str> = r.orphaned_sidecars.iter().map(|(id, _)| id.0.as_str()).collect();
        let mut blobs: Vec<&str> = r.orphaned_blobs.iter().map(|(id, _)| id.0.as_str()).collect();
        blobs.sort();
        assert_eq!(sidecars, [ID]);
        assert_eq!(blobs, [PART1, PART2]);
    }

    #[tokio::test]
    async fn unfinished_upload_parts_are_live_and_the_upload_is_listed() {
        let f = Fixture::new().await;
        let initiated = chrono::Utc::now() - chrono::Duration::days(3);
        f.store
            .create_multipart_upload(&MultipartUploadRecord {
                upload_id: "up-1".into(),
                bucket: "bucket".into(),
                key: "backup.tar".into(),
                content_type: None,
                initiated_at: initiated,
                metadata: HashMap::new(),
                checksum_algorithm: None,
                checksum_type: None,
                initiator: "root".to_string(),
            })
            .await
            .unwrap();
        for (n, part) in [(1, PART1), (2, PART2)] {
            f.write_blob(part, b"0123456789").await;
            f.store
                .put_part(&PartRecord {
                    upload_id: "up-1".into(),
                    part_number: n,
                    blob_id: BlobId(part.into()),
                    size: 10,
                    etag: md5_hex(b"0123456789"),
                    checksum_value: None,
                    last_modified: None,
                })
                .await
                .unwrap();
        }

        let r = f.scan(&opts()).await;
        assert_eq!(r.issue_count(), 0, "parts of an upload are live: {r:?}");
        assert_eq!(r.unfinished_uploads.len(), 1);
        let u = &r.unfinished_uploads[0];
        assert_eq!((u.key.as_str(), u.upload_id.as_str(), u.parts, u.bytes), ("backup.tar", "up-1", 2, 20));
        assert_eq!(run_fsck(&f.config, &opts(), None).await.unwrap(), 0, "unfinished uploads are informational");
    }

    #[tokio::test]
    async fn delete_markers_are_not_missing_blobs() {
        let f = Fixture::new().await;
        f.store.set_bucket_config("bucket", "versioning", "Enabled").await.unwrap();
        f.put_object(ID, "key.txt", b"hello").await;
        f.store.delete_object("bucket", "key.txt").await.unwrap();

        assert_eq!(f.scan(&opts()).await.issue_count(), 0);
    }

    #[tokio::test]
    async fn checksum_mismatch_is_reported() {
        let f = Fixture::new().await;
        f.put_object(ID, "key.txt", b"hello").await;
        fs::write(f.blob_path(ID), b"CORRUPTED DATA").await.unwrap();

        let r = f.scan(&FsckOptions { verify_checksums: true, grace: Duration::ZERO }).await;
        assert_eq!(r.checksum_errors.len(), 1);
        assert_eq!(r.checksum_errors[0].expected, md5_hex(b"hello"));
    }

    #[tokio::test]
    async fn checksum_skips_multipart_and_compressed_objects() {
        let f = Fixture::new().await;
        f.put_composite(ID, "large.bin").await;
        // A compressed object: the ETag hashes the plaintext, the file holds
        // the compressed bytes.
        let mut meta = sidecar("small.txt", 5, &md5_hex(b"hello"));
        meta.compression = Some(BlobCompressionInfo {
            algorithm: CompressionAlgorithm::Zstd,
            chunk_size: 65536,
            original_size: 5,
            compressed_size: 16,
        });
        f.write_blob(ID2, b"compressed bytes").await;
        f.write_sidecar(ID2, &meta).await;
        f.store.put_object(&record(ID2, "small.txt", 5, &md5_hex(b"hello"))).await.unwrap();

        let r = f.scan(&FsckOptions { verify_checksums: true, grace: Duration::ZERO }).await;
        assert!(r.checksum_errors.is_empty(), "{:?}", r.checksum_errors);
    }

    #[tokio::test]
    async fn missing_database_is_an_error_not_an_empty_scan() {
        let f = Fixture::new().await;
        drop(f.store);
        fs::remove_file(f.config.storage.db_path()).await.unwrap();

        let err = run_fsck(&f.config, &opts(), None).await.unwrap_err();
        assert!(err.to_string().contains("Database does not exist"), "{err}");
    }

    // -- Repair --

    use repair::Answer;
    use std::collections::VecDeque;
    use repair::InterruptState;
    use std::sync::atomic::AtomicBool;

    type Hook = Box<dyn FnOnce()>;

    /// Answers from a script; a hook runs while the "operator" is thinking,
    /// to change the state between the scan and the fix. Panics on a question
    /// the script did not expect.
    #[derive(Default)]
    struct Scripted {
        answers: VecDeque<(Answer, Option<Hook>)>,
        asked: Vec<String>,
    }

    impl Scripted {
        fn new(answers: &[Answer]) -> Self {
            Scripted {
                answers: answers.iter().map(|a| (*a, None)).collect(),
                asked: Vec::new(),
            }
        }
        fn with_hook(mut self, answer: Answer, hook: impl FnOnce() + 'static) -> Self {
            self.answers.push_back((answer, Some(Box::new(hook))));
            self
        }
    }

    impl Prompter for Scripted {
        fn ask(&mut self, question: &str) -> Answer {
            self.asked.push(question.to_string());
            let (answer, hook) = self
                .answers
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected question: {question}"));
            if let Some(hook) = hook {
                hook();
            }
            answer
        }
    }

    fn ropts() -> RepairOptions {
        RepairOptions { yes: false, delete: false, abort_uploads_older_than: None, cluster: false }
    }

    impl Fixture {
        async fn repair(&self, ropts: &RepairOptions, prompter: &mut Scripted) -> i32 {
            let state = InterruptState::default();
            check(&self.config, &opts(), Some((ropts, prompter, &state))).await.unwrap()
        }

        fn data_dir(&self) -> PathBuf {
            PathBuf::from(&self.config.storage.data_dir)
        }

        /// Every file under `lost+found`, relative to its run directory.
        fn quarantined(&self) -> Vec<PathBuf> {
            let root = self.data_dir().join("lost+found");
            let mut out = Vec::new();
            let Ok(runs) = std::fs::read_dir(&root) else { return out };
            for run in runs {
                let run = run.unwrap().path();
                let mut stack = vec![run.clone()];
                while let Some(d) = stack.pop() {
                    for e in std::fs::read_dir(&d).unwrap() {
                        let p = e.unwrap().path();
                        if p.is_dir() {
                            stack.push(p);
                        } else {
                            out.push(p.strip_prefix(&run).unwrap().to_path_buf());
                        }
                    }
                }
            }
            out.sort();
            out
        }

        fn rel(&self, p: &Path) -> PathBuf {
            p.strip_prefix(self.config.storage.blobs_dir()).unwrap().to_path_buf()
        }
    }

    #[tokio::test]
    async fn repair_removes_stale_temp_files_without_asking() {
        let f = Fixture::new().await;
        f.write_file_next_to(ID, &format!("{ID}.tmp")).await;
        f.write_file_next_to(ID2, &format!("{ID2}.rewrite.tmp")).await;

        let mut p = Scripted::default();
        assert_eq!(f.repair(&ropts(), &mut p).await, 1, "the rewrite recovery file is left");
        assert!(!f.blob_path(ID).with_file_name(format!("{ID}.tmp")).exists());
        assert!(f.blob_path(ID2).with_file_name(format!("{ID2}.rewrite.tmp")).exists(), "never touched");
        assert!(p.asked.is_empty());
    }

    #[tokio::test]
    async fn repair_quarantines_an_orphaned_blob_with_its_sidecar() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan").await;
        f.write_sidecar(ID, &sidecar("gone.txt", 6, &md5_hex(b"orphan"))).await;

        let mut p = Scripted::new(&[Answer::Yes]);
        assert_eq!(f.repair(&ropts(), &mut p).await, 0);
        assert!(!f.blob_path(ID).exists());
        let sidecar_path = f.fs.sidecar_path(&BlobId(ID.into()));
        assert!(!sidecar_path.exists());
        assert_eq!(f.quarantined(), vec![f.rel(&f.blob_path(ID)), f.rel(&sidecar_path)]);
        assert!(p.asked[0].contains("Move to lost+found"), "{:?}", p.asked);
    }

    #[tokio::test]
    async fn repair_with_delete_removes_orphans() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan").await;

        let mut p = Scripted::new(&[Answer::Yes]);
        let ro = RepairOptions { delete: true, ..ropts() };
        assert_eq!(f.repair(&ro, &mut p).await, 0);
        assert!(!f.blob_path(ID).exists());
        assert!(f.quarantined().is_empty());
    }

    #[tokio::test]
    async fn repair_declined_leaves_the_file_and_the_issue() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan").await;

        let mut p = Scripted::new(&[Answer::No]);
        assert_eq!(f.repair(&ropts(), &mut p).await, 1);
        assert!(f.blob_path(ID).exists());
    }

    #[tokio::test]
    async fn repair_all_answers_the_rest_of_the_category() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan 1").await;
        f.write_blob(ID2, b"orphan 2").await;

        let mut p = Scripted::new(&[Answer::All]);
        assert_eq!(f.repair(&ropts(), &mut p).await, 0);
        assert_eq!(p.asked.len(), 1);
        assert!(!f.blob_path(ID).exists() && !f.blob_path(ID2).exists());
    }

    #[tokio::test]
    async fn repair_quit_stops_everything() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan 1").await;
        f.write_blob(ID2, b"orphan 2").await;
        let mut bad = sidecar("k", 1, "x");
        bad.key = "wrong".into();
        f.put_object(PART1, "k", b"data").await;
        f.write_sidecar(PART1, &bad).await;

        let mut p = Scripted::new(&[Answer::Quit]);
        assert_eq!(f.repair(&ropts(), &mut p).await, 1);
        assert_eq!(p.asked.len(), 1, "no question after q: {:?}", p.asked);
        assert!(f.blob_path(ID).exists() && f.blob_path(ID2).exists());
    }

    #[tokio::test]
    async fn repair_stops_when_interrupted() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan").await;
        f.write_file_next_to(ID2, &format!("{ID2}.tmp")).await;

        let state = InterruptState { interrupted: AtomicBool::new(true), ..Default::default() };
        let mut p = Scripted::default();
        let code = check(&f.config, &opts(), Some((&ropts(), &mut p, &state))).await.unwrap();
        assert_eq!(code, 1);
        assert!(f.blob_path(ID).exists());
        assert!(f.blob_path(ID2).with_file_name(format!("{ID2}.tmp")).exists());
    }

    #[tokio::test]
    async fn repair_deletes_a_lost_version_and_reclaims_its_leftovers_in_the_same_run() {
        let f = Fixture::new().await;
        f.put_object(ID, "lost.txt", b"hello").await;
        fs::remove_file(f.blob_path(ID)).await.unwrap(); // the sidecar stays

        // 1) delete the row, 2) the rescan finds the sidecar orphaned.
        let mut p = Scripted::new(&[Answer::Yes, Answer::Yes]);
        assert_eq!(f.repair(&ropts(), &mut p).await, 0);
        assert!(f.store.get_object("bucket", "lost.txt").await.unwrap().is_none());
        assert!(p.asked[0].contains("Delete the row of bucket/lost.txt"), "{:?}", p.asked);
        assert!(p.asked[1].contains("orphaned sidecar"), "{:?}", p.asked);
        assert_eq!(f.quarantined(), vec![f.rel(&f.fs.sidecar_path(&BlobId(ID.into())))]);
        assert_eq!(f.scan(&opts()).await.issue_count(), 0);
    }

    #[tokio::test]
    async fn repair_keeps_a_version_whose_blob_came_back() {
        let f = Fixture::new().await;
        f.put_object(ID, "back.txt", b"hello").await;
        let blob = f.blob_path(ID);
        fs::remove_file(&blob).await.unwrap();

        let restore = blob.clone();
        let mut p = Scripted::default().with_hook(Answer::Yes, move || std::fs::write(restore, b"hello").unwrap());
        assert_eq!(f.repair(&ropts(), &mut p).await, 0);
        assert!(f.store.get_object("bucket", "back.txt").await.unwrap().is_some(), "the row must survive");
    }

    #[tokio::test]
    async fn repair_keeps_a_lost_composite_until_asked_and_removes_it_when_told() {
        let f = Fixture::new().await;
        f.put_composite(ID, "large.bin").await;
        fs::remove_file(f.blob_path(PART2)).await.unwrap();

        // Row deleted, then the rescan finds the composite sidecar and the
        // surviving part orphaned (one question each category, answered all).
        let mut p = Scripted::new(&[Answer::Yes, Answer::All, Answer::All]);
        assert_eq!(f.repair(&ropts(), &mut p).await, 0, "{:?}", p.asked);
        assert!(f.store.get_object("bucket", "large.bin").await.unwrap().is_none());
        assert!(!f.blob_path(PART1).exists());
        assert_eq!(f.scan(&opts()).await.issue_count(), 0);
    }

    #[tokio::test]
    async fn repair_skips_an_orphan_modified_since_the_scan() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan").await;
        // Let the file age past the cutoff taken when the repair starts.
        tokio::time::sleep(Duration::from_millis(20)).await;

        let path = f.blob_path(ID);
        let mut p = Scripted::default().with_hook(Answer::Yes, move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            std::fs::write(path, b"being written again").unwrap();
        });
        assert_eq!(f.repair(&ropts(), &mut p).await, 1);
        assert!(f.blob_path(ID).exists());
    }

    #[tokio::test]
    async fn repair_rewrites_a_sidecar_from_its_row_keeping_the_part_list() {
        let f = Fixture::new().await;
        f.put_composite(ID, "large.bin").await;
        let id = BlobId(ID.into());
        let mut meta = RawBlobOps::read_sidecar(&f.fs, &id).await.unwrap().unwrap();
        meta.key = "stale-key".into();
        f.write_sidecar(ID, &meta).await;

        let mut p = Scripted::new(&[Answer::Yes]);
        assert_eq!(f.repair(&ropts(), &mut p).await, 0);
        let fixed = RawBlobOps::read_sidecar(&f.fs, &id).await.unwrap().unwrap();
        assert_eq!(fixed.key, "large.bin");
        assert_eq!(fixed.composite.map(|c| c.len()), Some(2), "part list kept");
    }

    async fn create_upload(f: &Fixture, upload_id: &str, age_days: i64, part: &str) {
        f.store
            .create_multipart_upload(&MultipartUploadRecord {
                upload_id: upload_id.into(),
                bucket: "bucket".into(),
                key: format!("{upload_id}.tar"),
                content_type: None,
                initiated_at: chrono::Utc::now() - chrono::Duration::days(age_days),
                metadata: HashMap::new(),
                checksum_algorithm: None,
                checksum_type: None,
                initiator: "root".to_string(),
            })
            .await
            .unwrap();
        f.write_blob(part, b"0123456789").await;
        f.store
            .put_part(&PartRecord {
                upload_id: upload_id.into(),
                part_number: 1,
                blob_id: BlobId(part.into()),
                size: 10,
                etag: md5_hex(b"0123456789"),
                checksum_value: None,
                last_modified: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn repair_aborts_only_uploads_older_than_the_flag() {
        let f = Fixture::new().await;
        create_upload(&f, "old", 10, PART1).await;
        create_upload(&f, "new", 1, PART2).await;

        let ro = RepairOptions { abort_uploads_older_than: Some(chrono::Duration::days(7)), ..ropts() };
        let mut p = Scripted::new(&[Answer::Yes]);
        assert_eq!(f.repair(&ro, &mut p).await, 0);
        assert_eq!(p.asked.len(), 1, "{:?}", p.asked);
        assert!(f.store.get_multipart_upload("old").await.unwrap().is_none());
        assert!(!f.blob_path(PART1).exists(), "parts of the aborted upload are deleted");
        assert!(f.store.get_multipart_upload("new").await.unwrap().is_some());
        assert!(f.blob_path(PART2).exists());
    }

    #[tokio::test]
    async fn repair_without_the_flag_never_aborts_uploads() {
        let f = Fixture::new().await;
        create_upload(&f, "old", 10, PART1).await;

        let mut p = Scripted::default();
        assert_eq!(f.repair(&ropts(), &mut p).await, 0);
        assert!(f.store.get_multipart_upload("old").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn repair_on_a_cluster_node_fixes_files_but_never_rows_or_uploads() {
        let f = Fixture::new().await;
        f.store.put_object(&record(ID, "lost.txt", 5, "abc")).await.unwrap();
        create_upload(&f, "old", 10, PART1).await;
        f.write_blob(ID2, b"orphan").await;

        let ro = RepairOptions {
            cluster: true,
            abort_uploads_older_than: Some(chrono::Duration::days(1)),
            ..ropts()
        };
        let mut p = Scripted::new(&[Answer::Yes]); // only the orphan is asked
        assert_eq!(f.repair(&ro, &mut p).await, 1, "the missing blob is left");
        assert!(p.asked[0].contains("orphaned blob"), "{:?}", p.asked);
        assert!(f.store.get_object("bucket", "lost.txt").await.unwrap().is_some());
        assert!(f.store.get_multipart_upload("old").await.unwrap().is_some());
        assert!(!f.blob_path(ID2).exists());
    }

    #[tokio::test]
    async fn repair_leaves_orphans_alone_while_a_sidecar_is_unreadable() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"maybe a part").await;
        let path = f.fs.sidecar_path(&BlobId(ID2.into()));
        fs::create_dir_all(path.parent().unwrap()).await.unwrap();
        fs::write(&path, b"{ not json").await.unwrap();

        let mut p = Scripted::default();
        assert_eq!(f.repair(&ropts(), &mut p).await, 1);
        assert!(f.blob_path(ID).exists());
    }

    #[tokio::test]
    async fn repair_refuses_while_a_rewrite_is_unfinished() {
        let f = Fixture::new().await;
        crate::rewrite_marker::begin(&f.data_dir(), "encrypt-existing", vec![]).unwrap();

        let state = InterruptState::default();
        let mut p = Scripted::default();
        let err = check(&f.config, &opts(), Some((&ropts(), &mut p, &state))).await.unwrap_err();
        assert!(err.to_string().contains("refusing to repair"), "{err}");
    }

    #[tokio::test]
    async fn files_that_are_not_arca_blobs_are_reported_with_their_size_and_never_repaired() {
        let f = Fixture::new().await;
        let blobs = f.config.storage.blobs_dir();
        fs::write(blobs.join("huge.bin"), vec![0u8; 4096]).await.unwrap();
        fs::write(blobs.join("backup.meta"), b"{}").await.unwrap();

        let r = f.scan(&opts()).await;
        assert!(r.orphaned_blobs.is_empty() && r.orphaned_sidecars.is_empty());
        assert!(r.unreadable_sidecars.is_empty());
        let mut names: Vec<_> = r
            .foreign_files
            .iter()
            .map(|d| (d.path.file_name().unwrap().to_str().unwrap().to_string(), d.size))
            .collect();
        names.sort();
        assert_eq!(names, vec![("backup.meta".to_string(), 2), ("huge.bin".to_string(), 4096)]);
        // Informational: not an inconsistency, and --repair must leave them.
        assert_eq!(r.issue_count(), 0);
        let ro = RepairOptions { yes: true, ..ropts() };
        assert_eq!(f.repair(&ro, &mut Scripted::default()).await, 0);
        assert!(blobs.join("huge.bin").exists() && blobs.join("backup.meta").exists());
    }

    #[tokio::test]
    async fn repair_with_yes_fixes_everything_and_a_second_run_finds_nothing() {
        let f = Fixture::new().await;
        f.put_object(ID, "lost.txt", b"hello").await;
        fs::remove_file(f.blob_path(ID)).await.unwrap();
        f.write_blob(ID2, b"orphan").await;
        f.write_file_next_to(PART1, &format!("{PART1}.tmp")).await;

        let ro = RepairOptions { yes: true, ..ropts() };
        assert_eq!(f.repair(&ro, &mut Scripted::default()).await, 0);
        assert_eq!(f.repair(&ro, &mut Scripted::default()).await, 0);
        assert_eq!(f.scan(&opts()).await.issue_count(), 0);
    }

    #[tokio::test]
    async fn repair_without_a_terminal_needs_yes() {
        let f = Fixture::new().await;
        let err = run_fsck(&f.config, &opts(), Some(&ropts())).await.unwrap_err();
        assert!(err.to_string().contains("--yes"), "{err}");
    }

    #[test]
    fn a_cluster_node_waits_at_least_the_tombstone_grace() {
        let cluster: crate::config::ClusterConfig = toml::from_str(
            "enabled = true\ncluster_id = \"c\"\nsecret = \"s\"\ntombstone_grace_seconds = 600\n",
        )
        .unwrap();
        let short = Duration::from_secs(60);
        assert_eq!(effective_grace(short, Some(&cluster)), Duration::from_secs(600));
        assert_eq!(effective_grace(Duration::from_secs(900), Some(&cluster)), Duration::from_secs(900));
        assert_eq!(effective_grace(short, None), short);
        let off = crate::config::ClusterConfig { enabled: false, ..cluster };
        assert_eq!(effective_grace(short, Some(&off)), short);
    }

    #[tokio::test]
    async fn ctrl_c_at_a_question_is_not_during_a_fix() {
        let f = Fixture::new().await;
        f.write_blob(ID, b"orphan").await;
        let state = std::sync::Arc::new(InterruptState::default());
        let seen = state.clone();
        let mut p = Scripted::default().with_hook(Answer::Yes, move || {
            assert!(!seen.in_fix.load(std::sync::atomic::Ordering::SeqCst), "Ctrl+C must exit at once here");
        });
        check(&f.config, &opts(), Some((&ropts(), &mut p, &state))).await.unwrap();
        assert!(!state.in_fix.load(std::sync::atomic::Ordering::SeqCst), "no fix left open");
    }

    #[test]
    fn human_bytes_uses_binary_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(9 * 1024 * 1024 * 1024), "9.0 GiB");
    }
}
