//! Filesystem consistency check: compare database records against blob files on disk.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use tokio::fs;
use tokio::io::AsyncReadExt;

use arca_core::store::blob::SidecarMeta;
use arca_core::store::MetadataStore;
use arca_core::types::{BlobId, ObjectRecord};

use crate::config::Config;

/// Result of the fsck run.
struct FsckReport {
    orphaned_blobs: Vec<BlobId>,
    missing_blobs: Vec<(String, String, BlobId)>, // (bucket, key, blob_id)
    sidecar_mismatches: Vec<SidecarMismatch>,
    orphaned_sidecars: Vec<BlobId>,
    stale_tmp_files: Vec<PathBuf>,
    checksum_errors: Vec<ChecksumError>,
}

struct SidecarMismatch {
    blob_id: BlobId,
    details: String,
}

struct ChecksumError {
    blob_id: BlobId,
    expected: String,
    actual: String,
}

impl FsckReport {
    fn issue_count(&self) -> usize {
        self.orphaned_blobs.len()
            + self.missing_blobs.len()
            + self.sidecar_mismatches.len()
            + self.orphaned_sidecars.len()
            + self.stale_tmp_files.len()
            + self.checksum_errors.len()
    }
}

/// Runs the `arca fsck` command. Returns exit code (0 = clean, 1 = issues found).
pub async fn run_fsck(config: &Config, verify_checksums: bool) -> Result<i32> {
    let blobs_dir = config.storage.blobs_dir();
    let db_path = config.storage.db_path();

    if !db_path.exists() {
        anyhow::bail!("Database does not exist: {}", db_path.display());
    }
    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }

    println!("Opening database...");
    let store = arca_storage::SqliteStore::open(&db_path).await?;

    println!("Scanning {}...", blobs_dir.display());

    // Phase 1: Walk filesystem and categorize files.
    let mut disk_blobs: HashSet<BlobId> = HashSet::new();
    let mut disk_sidecars: HashMap<BlobId, PathBuf> = HashMap::new();
    let mut stale_tmp_files: Vec<PathBuf> = Vec::new();

    walk_blobs_dir(&blobs_dir, &mut disk_blobs, &mut disk_sidecars, &mut stale_tmp_files).await?;

    println!(
        "Found {} blob(s), {} sidecar(s), {} tmp file(s) on disk",
        disk_blobs.len(),
        disk_sidecars.len(),
        stale_tmp_files.len()
    );

    // Phase 2: Load all objects from DB.
    let db_objects = load_all_objects(&store).await?;
    // Exclude delete markers (blob_id="", no physical blob).
    let real_objects: Vec<&ObjectRecord> = db_objects.iter().filter(|o| !o.is_delete_marker).collect();
    let db_blob_ids: HashSet<BlobId> =
        real_objects.iter().map(|o| o.blob_id.clone()).collect();

    let dm_count = db_objects.len() - real_objects.len();
    println!(
        "Found {} object version(s) in database ({} delete markers)",
        db_objects.len(),
        dm_count,
    );
    println!();

    // Check A: Orphaned blobs (on disk but not in DB).
    let orphaned_blobs: Vec<BlobId> = disk_blobs
        .difference(&db_blob_ids)
        .cloned()
        .collect();

    // Check B: Missing blobs (in DB but not on disk). Skip delete markers.
    let missing_blobs: Vec<(String, String, BlobId)> = real_objects
        .iter()
        .filter(|o| !disk_blobs.contains(&o.blob_id))
        .map(|o| (o.bucket.clone(), o.key.clone(), o.blob_id.clone()))
        .collect();

    // Check C: Sidecar mismatches (sidecar data doesn't match DB).
    let sidecar_mismatches = check_sidecar_mismatches(&disk_sidecars, &db_objects).await;

    // Check D: Orphaned sidecars (.meta exists, no corresponding blob).
    let orphaned_sidecars: Vec<BlobId> = disk_sidecars
        .keys()
        .filter(|id| !disk_blobs.contains(*id))
        .cloned()
        .collect();

    // Check E: Stale tmp files — already collected during walk.

    // Check F: Checksum verification (optional).
    let checksum_errors = if verify_checksums {
        println!("Verifying checksums...");
        check_checksums(&blobs_dir, &db_objects, &disk_blobs).await
    } else {
        Vec::new()
    };

    let report = FsckReport {
        orphaned_blobs,
        missing_blobs,
        sidecar_mismatches,
        orphaned_sidecars,
        stale_tmp_files,
        checksum_errors,
    };

    // Print report.
    print_report(&report);

    if report.issue_count() == 0 {
        println!("No issues found.");
        Ok(0)
    } else {
        println!(
            "\n{} issue(s) found.",
            report.issue_count()
        );
        Ok(1)
    }
}

/// Recursively walks the blobs directory, categorizing files into blobs, sidecars, and tmps.
async fn walk_blobs_dir(
    dir: &Path,
    blobs: &mut HashSet<BlobId>,
    sidecars: &mut HashMap<BlobId, PathBuf>,
    tmp_files: &mut Vec<PathBuf>,
) -> Result<()> {
    let mut read_dir = fs::read_dir(dir)
        .await
        .with_context(|| format!("reading directory: {}", dir.display()))?;

    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;

        if file_type.is_dir() {
            Box::pin(walk_blobs_dir(&path, blobs, sidecars, tmp_files)).await?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        if let Some(id_str) = name.strip_suffix(".meta") {
            sidecars.insert(BlobId(id_str.to_string()), path);
        } else if name.ends_with(".tmp") {
            tmp_files.push(path);
        } else {
            // Assume it's a blob file — the filename is the blob_id (UUID).
            blobs.insert(BlobId(name.to_string()));
        }
    }

    Ok(())
}

/// Loads all object versions (including old versions and delete markers) from all buckets.
async fn load_all_objects(store: &arca_storage::SqliteStore) -> Result<Vec<ObjectRecord>> {
    let buckets = store.list_buckets().await?;
    let mut all_objects = Vec::new();

    for bucket in &buckets {
        let mut key_marker: Option<String> = None;
        loop {
            let objects = store
                .list_object_versions(&bucket.name, None, key_marker.as_deref(), None, 1000)
                .await?;
            if objects.is_empty() {
                break;
            }
            key_marker = Some(objects.last().unwrap().key.clone());
            all_objects.extend(objects);
        }
    }

    Ok(all_objects)
}

/// Checks for mismatches between sidecar files and DB records.
async fn check_sidecar_mismatches(
    sidecars: &HashMap<BlobId, PathBuf>,
    db_objects: &[ObjectRecord],
) -> Vec<SidecarMismatch> {
    // Index DB objects by blob_id for quick lookup.
    let db_by_blob: HashMap<&BlobId, &ObjectRecord> =
        db_objects.iter().map(|o| (&o.blob_id, o)).collect();

    let mut mismatches = Vec::new();

    for (blob_id, sidecar_path) in sidecars {
        let db_obj = match db_by_blob.get(blob_id) {
            Some(obj) => *obj,
            None => continue, // No DB record for this sidecar — checked elsewhere.
        };

        let json = match fs::read_to_string(sidecar_path).await {
            Ok(j) => j,
            Err(e) => {
                mismatches.push(SidecarMismatch {
                    blob_id: blob_id.clone(),
                    details: format!("could not read sidecar: {e}"),
                });
                continue;
            }
        };

        let meta: SidecarMeta = match serde_json::from_str(&json) {
            Ok(m) => m,
            Err(e) => {
                mismatches.push(SidecarMismatch {
                    blob_id: blob_id.clone(),
                    details: format!("malformed sidecar JSON: {e}"),
                });
                continue;
            }
        };

        let mut diffs = Vec::new();
        if meta.bucket != db_obj.bucket {
            diffs.push(format!(
                "bucket: sidecar={}, db={}",
                meta.bucket, db_obj.bucket
            ));
        }
        if meta.key != db_obj.key {
            diffs.push(format!("key: sidecar={}, db={}", meta.key, db_obj.key));
        }
        if meta.size != db_obj.size {
            diffs.push(format!("size: sidecar={}, db={}", meta.size, db_obj.size));
        }
        if meta.etag != db_obj.etag {
            diffs.push(format!(
                "etag: sidecar={}, db={}",
                meta.etag, db_obj.etag
            ));
        }

        if !diffs.is_empty() {
            mismatches.push(SidecarMismatch {
                blob_id: blob_id.clone(),
                details: diffs.join("; "),
            });
        }
    }

    mismatches
}

/// Verifies blob checksums against stored ETags.
async fn check_checksums(
    blobs_dir: &Path,
    db_objects: &[ObjectRecord],
    disk_blobs: &HashSet<BlobId>,
) -> Vec<ChecksumError> {
    let mut errors = Vec::new();

    for obj in db_objects {
        // Skip multipart objects (composite ETag ≠ MD5).
        if obj.etag.contains('-') {
            continue;
        }

        // Skip encrypted objects (on-disk ciphertext MD5 ≠ plaintext ETag).
        if obj.encryption_algorithm.is_some() {
            continue;
        }

        // Skip if blob doesn't exist on disk (already reported as missing).
        if !disk_blobs.contains(&obj.blob_id) {
            continue;
        }

        let blob_path = find_blob_path(blobs_dir, &obj.blob_id).await;
        let blob_path = match blob_path {
            Some(p) => p,
            None => continue,
        };

        match compute_md5(&blob_path).await {
            Ok(actual) if actual != obj.etag => {
                errors.push(ChecksumError {
                    blob_id: obj.blob_id.clone(),
                    expected: obj.etag.clone(),
                    actual,
                });
            }
            Ok(_) => {} // match
            Err(e) => {
                eprintln!(
                    "WARNING: could not read blob {} for checksum: {e}",
                    obj.blob_id
                );
            }
        }
    }

    errors
}

/// Finds a blob file on disk by walking directories. This avoids needing to
/// know the prefix_depth — we search for the blob_id filename.
async fn find_blob_path(blobs_dir: &Path, blob_id: &BlobId) -> Option<PathBuf> {
    find_file_recursive(blobs_dir, &blob_id.0).await
}

/// Recursively searches for a file with the given name under the directory.
async fn find_file_recursive(dir: &Path, filename: &str) -> Option<PathBuf> {
    let mut read_dir = fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = read_dir.next_entry().await {
        let path = entry.path();
        if let Ok(ft) = entry.file_type().await {
            if ft.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name == filename {
                        return Some(path);
                    }
                }
            } else if ft.is_dir() {
                if let Some(found) = Box::pin(find_file_recursive(&path, filename)).await {
                    return Some(found);
                }
            }
        }
    }
    None
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

fn print_report(report: &FsckReport) {
    for blob_id in &report.orphaned_blobs {
        println!("ORPHANED_BLOB  {blob_id}  blob file exists but no DB record references it");
    }

    for (bucket, key, blob_id) in &report.missing_blobs {
        println!("MISSING_BLOB   {blob_id}  DB record {bucket}/{key} references missing blob file");
    }

    for m in &report.sidecar_mismatches {
        println!("SIDECAR_MISMATCH  {}  {}", m.blob_id, m.details);
    }

    for blob_id in &report.orphaned_sidecars {
        println!("ORPHANED_SIDECAR  {blob_id}  .meta file exists but no corresponding blob file");
    }

    for path in &report.stale_tmp_files {
        println!("STALE_TMP  {}  leftover temporary file", path.display());
    }

    for e in &report.checksum_errors {
        println!(
            "CORRUPT  {}  expected={}, actual={}",
            e.blob_id, e.expected, e.actual
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ServerConfig, StorageConfig};
    use arca_core::store::blob::SidecarMeta;

    /// Creates a test config pointing at a temp directory.
    fn test_config(dir: &Path) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1".to_string(),
                port: 9000,
                domain: None,
                region: None,
                tls: None,
                limits: None,
                cache: None,
            },
            storage: StorageConfig {
                data_dir: dir.to_str().unwrap().to_string(),
                blob_prefix_depth: 2,
                metadata_backend: "sqlite".to_string(),
                postgres: None,
            },
            encryption: None,
            monitoring: None,
            notifications: None,
        }
    }

    /// Computes MD5 hex of given data.
    fn md5_hex(data: &[u8]) -> String {
        hex::encode(Md5::digest(data))
    }

    /// Creates a blob file in the sharded directory structure.
    async fn write_blob(blobs_dir: &Path, blob_id: &str, content: &[u8]) {
        let hex_chars: String = blob_id.chars().filter(|c| *c != '-').collect();
        let dir = blobs_dir.join(&hex_chars[0..2]).join(&hex_chars[2..4]);
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(dir.join(blob_id), content).await.unwrap();
    }

    /// Creates a sidecar file alongside a blob.
    async fn write_sidecar(blobs_dir: &Path, blob_id: &str, meta: &SidecarMeta) {
        let hex_chars: String = blob_id.chars().filter(|c| *c != '-').collect();
        let dir = blobs_dir.join(&hex_chars[0..2]).join(&hex_chars[2..4]);
        fs::create_dir_all(&dir).await.unwrap();
        let json = serde_json::to_string_pretty(meta).unwrap();
        fs::write(dir.join(format!("{blob_id}.meta")), json)
            .await
            .unwrap();
    }

    /// Creates a tmp file in the sharded directory structure.
    async fn write_tmp(blobs_dir: &Path, blob_id: &str) {
        let hex_chars: String = blob_id.chars().filter(|c| *c != '-').collect();
        let dir = blobs_dir.join(&hex_chars[0..2]).join(&hex_chars[2..4]);
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(dir.join(format!("{blob_id}.tmp")), b"partial")
            .await
            .unwrap();
    }

    /// Sets up a consistent state: blob + sidecar on disk, matching DB record.
    async fn setup_clean_state(
        config: &Config,
        store: &arca_storage::SqliteStore,
        blob_id: &str,
        bucket: &str,
        key: &str,
        content: &[u8],
    ) {
        let blobs_dir = config.storage.blobs_dir();
        let etag = md5_hex(content);

        // Ensure bucket exists (ignore already-exists error).
        let _ = store.create_bucket(bucket).await;

        write_blob(&blobs_dir, blob_id, content).await;

        let meta = SidecarMeta {
            bucket: bucket.into(),
            key: key.into(),
            size: content.len() as u64,
            etag: etag.clone(),
            content_type: Some("text/plain".into()),
            last_modified: "2024-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            version_id: None,
        };
        write_sidecar(&blobs_dir, blob_id, &meta).await;

        let record = ObjectRecord {
            bucket: bucket.into(),
            key: key.into(),
            blob_id: BlobId(blob_id.into()),
            size: content.len() as u64,
            etag,
            content_type: Some("text/plain".into()),
            last_modified: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            metadata: HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
        };
        store.put_object(&record).await.unwrap();
    }

    #[tokio::test]
    async fn fsck_clean_state() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();

        let id = "550e8400-e29b-41d4-a716-446655440000";
        setup_clean_state(&config, &store, id, "bucket", "key.txt", b"hello").await;
        drop(store);

        let exit_code = run_fsck(&config, false).await.unwrap();
        assert_eq!(exit_code, 0);
    }

    #[tokio::test]
    async fn fsck_orphaned_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        // Create DB (empty).
        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        drop(store);

        // Write a blob with no corresponding DB record.
        let id = "550e8400-e29b-41d4-a716-446655440000";
        write_blob(&blobs_dir, id, b"orphan data").await;

        let exit_code = run_fsck(&config, false).await.unwrap();
        assert_eq!(exit_code, 1);
    }

    #[tokio::test]
    async fn fsck_missing_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        store.create_bucket("bucket").await.unwrap();

        // Insert DB record with no blob on disk.
        let record = ObjectRecord {
            bucket: "bucket".into(),
            key: "missing.txt".into(),
            blob_id: BlobId("550e8400-e29b-41d4-a716-446655440000".into()),
            size: 10,
            etag: "abc123".into(),
            content_type: None,
            last_modified: chrono::Utc::now(),
            metadata: HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
        };
        store.put_object(&record).await.unwrap();
        drop(store);

        let exit_code = run_fsck(&config, false).await.unwrap();
        assert_eq!(exit_code, 1);
    }

    #[tokio::test]
    async fn fsck_sidecar_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let id = "550e8400-e29b-41d4-a716-446655440000";

        // Create a clean state first.
        setup_clean_state(&config, &store, id, "bucket", "key.txt", b"hello").await;

        // Now overwrite the sidecar with different data.
        let bad_meta = SidecarMeta {
            bucket: "wrong-bucket".into(),
            key: "key.txt".into(),
            size: 5,
            etag: md5_hex(b"hello"),
            content_type: None,
            last_modified: "2024-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            version_id: None,
        };
        write_sidecar(&blobs_dir, id, &bad_meta).await;
        drop(store);

        let exit_code = run_fsck(&config, false).await.unwrap();
        assert_eq!(exit_code, 1);
    }

    #[tokio::test]
    async fn fsck_stale_tmp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        drop(store);

        let id = "550e8400-e29b-41d4-a716-446655440000";
        write_tmp(&blobs_dir, id).await;

        let exit_code = run_fsck(&config, false).await.unwrap();
        assert_eq!(exit_code, 1);
    }

    #[tokio::test]
    async fn fsck_checksum_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let id = "550e8400-e29b-41d4-a716-446655440000";

        // Set up clean state.
        setup_clean_state(&config, &store, id, "bucket", "key.txt", b"hello").await;

        // Corrupt the blob file (overwrite with different content).
        let hex_chars: String = id.chars().filter(|c| *c != '-').collect();
        let blob_path = blobs_dir
            .join(&hex_chars[0..2])
            .join(&hex_chars[2..4])
            .join(id);
        fs::write(&blob_path, b"CORRUPTED DATA").await.unwrap();
        drop(store);

        let exit_code = run_fsck(&config, true).await.unwrap();
        assert_eq!(exit_code, 1);
    }

    #[tokio::test]
    async fn fsck_multipart_etag_skipped_in_checksum_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        store.create_bucket("bucket").await.unwrap();

        let id = "550e8400-e29b-41d4-a716-446655440000";
        write_blob(&blobs_dir, id, b"multipart assembled data").await;

        let meta = SidecarMeta {
            bucket: "bucket".into(),
            key: "large.bin".into(),
            size: 24,
            etag: "d41d8cd98f00b204e9800998ecf8427e-3".into(),
            content_type: None,
            last_modified: "2024-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            version_id: None,
        };
        write_sidecar(&blobs_dir, id, &meta).await;

        // Insert DB record with multipart etag.
        let record = ObjectRecord {
            bucket: "bucket".into(),
            key: "large.bin".into(),
            blob_id: BlobId(id.into()),
            size: 24,
            etag: "d41d8cd98f00b204e9800998ecf8427e-3".into(),
            content_type: None,
            last_modified: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            metadata: HashMap::new(),
            encryption_algorithm: None,
            encryption_key_id: None,
            owner: "root".to_string(),
            version_id: None,
            is_latest: true,
            is_delete_marker: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
        };
        store.put_object(&record).await.unwrap();
        drop(store);

        // With checksum verification ON, multipart objects should be skipped gracefully.
        let exit_code = run_fsck(&config, true).await.unwrap();
        assert_eq!(exit_code, 0);
    }
}
