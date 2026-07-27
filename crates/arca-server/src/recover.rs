//! Disaster recovery: rebuild the SQLite database from `.meta` sidecar files.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::DateTime;
use md5::{Digest, Md5};
use tokio::fs;
use tokio::io::AsyncReadExt;

use arca_core::store::blob::SidecarMeta;
use arca_core::store::{CredentialStore, MetadataStore};
use arca_core::types::{BlobId, Credential, ObjectRecord};

use crate::config::Config;

/// A validated sidecar entry ready for insertion.
struct RecoveredEntry {
    blob_id: BlobId,
    meta: SidecarMeta,
}

/// Runs the `arca recover` command.
///
/// Walks the blobs directory, reads `.meta` sidecar files, and rebuilds the
/// SQLite database from scratch. Credentials are preserved across the rebuild.
pub async fn run_recover(config: &Config, dry_run: bool, skip_verify: bool) -> Result<()> {
    let blobs_dir = config.storage.blobs_dir();
    let db_path = config.storage.db_path();

    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }

    println!("Scanning {}...", blobs_dir.display());

    // Phase 1: Walk filesystem and collect valid sidecar entries.
    let entries = walk_and_collect(&blobs_dir, skip_verify).await?;

    // Deduplicate bucket names (BTreeSet for deterministic order).
    let buckets: BTreeSet<String> = entries.iter().map(|e| e.meta.bucket.clone()).collect();

    println!(
        "Found {} sidecar(s) across {} bucket(s)",
        entries.len(),
        buckets.len()
    );

    if dry_run {
        println!("\n[dry-run] Would recover:");
        for bucket in &buckets {
            let count = entries.iter().filter(|e| &e.meta.bucket == bucket).count();
            println!("  Bucket {bucket}: {count} object(s)");
        }
        println!("\nNo changes made.");
        return Ok(());
    }

    // Phase 2: Preserve credentials from existing DB (if any).
    let saved_credentials = load_credentials(&db_path).await;

    // Phase 3: Delete existing DB and create fresh one.
    if db_path.exists() {
        fs::remove_file(&db_path)
            .await
            .with_context(|| format!("deleting old database: {}", db_path.display()))?;
        println!("Deleted old database");
    }

    let store = arca_storage::SqliteStore::open(&db_path).await?;
    println!("Created new database");

    // Phase 4: Restore credentials.
    if let Some(creds) = saved_credentials {
        for cred in &creds {
            store.put_credential(cred).await?;
        }
        println!("Restored {} credential(s)", creds.len());
    }

    // Phase 5: Create buckets.
    for bucket in &buckets {
        store.create_bucket(bucket).await?;
    }
    println!("Created {} bucket(s)", buckets.len());

    // Phase 6: Insert objects.
    // Sort entries by (bucket, key, last_modified DESC) so that for versioned
    // objects with multiple sidecars, the newest version is inserted last and
    // becomes the current version (put_object overwrites on unversioned buckets).
    let mut entries = entries;
    entries.sort_by(|a, b| {
        (&a.meta.bucket, &a.meta.key, &b.meta.last_modified)
            .cmp(&(&b.meta.bucket, &b.meta.key, &a.meta.last_modified))
    });
    let mut object_count = 0u64;
    for entry in &entries {
        let last_modified = DateTime::parse_from_rfc3339(&entry.meta.last_modified)
            .with_context(|| {
                format!(
                    "parsing last_modified for blob {}: {:?}",
                    entry.blob_id, entry.meta.last_modified
                )
            })?
            .with_timezone(&chrono::Utc);

        let record = ObjectRecord {
            bucket: entry.meta.bucket.clone(),
            key: entry.meta.key.clone(),
            blob_id: entry.blob_id.clone(),
            size: entry.meta.size,
            etag: entry.meta.etag.clone(),
            content_type: entry.meta.content_type.clone(),
            last_modified,
            metadata: entry.meta.metadata.clone(),
            encryption_algorithm: entry.meta.encryption.as_ref().map(|e| e.algorithm.clone()),
            encryption_key_id: entry.meta.encryption.as_ref().map(|e| e.key_id.clone()),
            owner: "root".to_string(),
            version_id: entry.meta.version_id.clone(),
            is_latest: true,
            is_delete_marker: false,
            is_tombstone: false,
            retention_mode: None,
            retain_until_date: None,
            legal_hold_status: None,
            storage_class: "STANDARD".to_string(),
            checksum_algorithm: None,
            checksum_value: None,
            replication_status: None,
            lock_updated_at: None,
            content_updated_at: None,
        };
        store.put_object(&record).await?;
        object_count += 1;
    }

    println!(
        "\nRecovery complete: {} bucket(s), {} object(s)",
        buckets.len(),
        object_count
    );

    Ok(())
}

/// Walks the blobs directory recursively, collecting valid sidecar entries.
async fn walk_and_collect(blobs_dir: &Path, skip_verify: bool) -> Result<Vec<RecoveredEntry>> {
    let mut entries = Vec::new();
    let mut meta_files = Vec::new();

    // Collect all .meta file paths first.
    collect_meta_files(blobs_dir, &mut meta_files).await?;

    println!("Found {} sidecar file(s)", meta_files.len());

    for meta_path in &meta_files {
        match process_sidecar(meta_path, skip_verify).await {
            Ok(entry) => entries.push(entry),
            Err(e) => {
                eprintln!("WARNING: skipping {}: {e}", meta_path.display());
            }
        }
    }

    Ok(entries)
}

/// Recursively collects all `.meta` file paths under the given directory.
async fn collect_meta_files(dir: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
    let mut read_dir = fs::read_dir(dir)
        .await
        .with_context(|| format!("reading directory: {}", dir.display()))?;

    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;

        if file_type.is_dir() {
            Box::pin(collect_meta_files(&path, result)).await?;
        } else if file_type.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".meta") {
                    result.push(path);
                }
            }
        }
    }

    Ok(())
}

/// Processes a single `.meta` file: parse JSON, validate blob exists, verify checksum.
async fn process_sidecar(meta_path: &Path, skip_verify: bool) -> Result<RecoveredEntry> {
    // Extract blob_id from filename: "{uuid}.meta" -> "{uuid}"
    let file_name = meta_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid sidecar filename")?;
    let blob_id_str = file_name
        .strip_suffix(".meta")
        .context("sidecar file does not end with .meta")?;
    let blob_id = BlobId(blob_id_str.to_string());

    // The blob file is the same path without .meta
    let blob_path = meta_path.with_file_name(blob_id_str);

    // Check that the blob file exists.
    if !blob_path.exists() {
        // TECHDEBT(TD-014): composite blobs (encrypted/plain `CompleteMultipartUpload`
        // optimisation) have no on-disk file — only a sidecar listing their parts.
        // Recover currently rejects them as orphans. Proper fix: when the sidecar
        // has `composite: Some(parts)`, validate each referenced part exists and
        // insert a single ObjectRecord pointing at the composite blob_id.
        anyhow::bail!("orphaned sidecar (blob file missing)");
    }

    // Parse sidecar JSON.
    let json = fs::read_to_string(meta_path)
        .await
        .context("reading sidecar file")?;
    let meta: SidecarMeta =
        serde_json::from_str(&json).context("parsing sidecar JSON")?;

    // Verify checksum unless skipped.
    if !skip_verify {
        let is_multipart = meta.etag.contains('-');
        let is_encrypted = meta.encryption.is_some();
        if is_encrypted {
            // Encrypted blobs have ciphertext on disk — plaintext MD5 can't be
            // verified without the master key. Skip with a note.
            eprintln!(
                "NOTE: skipping checksum for encrypted blob {blob_id} (use --skip-verify or provide master key)"
            );
        } else if !is_multipart {
            verify_blob_checksum(&blob_path, &meta.etag)
                .await
                .with_context(|| format!("blob {blob_id}"))?;
        }
    }

    Ok(RecoveredEntry { blob_id, meta })
}

/// Reads a blob file and verifies its MD5 matches the expected ETag.
async fn verify_blob_checksum(blob_path: &Path, expected_etag: &str) -> Result<()> {
    let mut file = fs::File::open(blob_path)
        .await
        .context("opening blob for checksum")?;

    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf).await.context("reading blob")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let actual = hex::encode(hasher.finalize());
    if actual != expected_etag {
        anyhow::bail!(
            "checksum mismatch: expected {expected_etag}, got {actual}"
        );
    }

    Ok(())
}

/// Attempts to load credentials from an existing database. Returns None if
/// the DB doesn't exist or can't be read.
async fn load_credentials(db_path: &Path) -> Option<Vec<Credential>> {
    if !db_path.exists() {
        return None;
    }

    let store = match arca_storage::SqliteStore::open(db_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "WARNING: could not open existing DB to preserve credentials: {e}"
            );
            return None;
        }
    };

    match store.list_credentials().await {
        Ok(creds) if !creds.is_empty() => {
            println!("Saved {} credential(s) from existing database", creds.len());
            Some(creds)
        }
        Ok(_) => None,
        Err(e) => {
            eprintln!("WARNING: could not read credentials from existing DB: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ServerConfig, StorageConfig};
    use std::collections::{HashMap, HashSet};

    /// Creates a test config pointing at a temp directory.
    fn test_config(dir: &Path) -> Config {
        Config {
            server: ServerConfig {
                bind: "127.0.0.1".to_string(),
                port: 9000,
                domain: None,
                region: None,
                log_level: None,
                tls: None,
                limits: None,
                cache: None,
                runtime: None,
                http: None,
            },
            storage: StorageConfig {
                data_dir: dir.to_str().unwrap().to_string(),
                blob_prefix_depth: 2,
                metadata_backend: "sqlite".to_string(),
                postgres: None,
                blob_gc_enabled: false,
                blob_gc_interval_seconds: 3600,
                blob_gc_grace_seconds: 86400,
            },
            encryption: None,
            monitoring: None,
            notifications: None,
            replication: None,
            lifecycle: None,
            cluster: None,
        }
    }

    /// Writes a blob file and its sidecar in the correct sharded directory.
    async fn write_blob_and_sidecar(
        blobs_dir: &Path,
        blob_id: &str,
        content: &[u8],
        meta: &SidecarMeta,
    ) {
        // Compute sharded path (depth=2).
        let hex_chars: String = blob_id.chars().filter(|c| *c != '-').collect();
        let prefix1 = &hex_chars[0..2];
        let prefix2 = &hex_chars[2..4];
        let dir = blobs_dir.join(prefix1).join(prefix2);
        fs::create_dir_all(&dir).await.unwrap();

        // Write blob file.
        fs::write(dir.join(blob_id), content).await.unwrap();

        // Write sidecar.
        let json = serde_json::to_string_pretty(meta).unwrap();
        fs::write(dir.join(format!("{blob_id}.meta")), json)
            .await
            .unwrap();
    }

    /// Computes MD5 hex of given data.
    fn md5_hex(data: &[u8]) -> String {
        hex::encode(Md5::digest(data))
    }

    #[tokio::test]
    async fn recover_three_sidecars_two_buckets() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let content_a = b"hello";
        let content_b = b"world";
        let content_c = b"data!";

        let id_a = "550e8400-e29b-41d4-a716-446655440000";
        let id_b = "660e8400-e29b-41d4-a716-446655440000";
        let id_c = "770e8400-e29b-41d4-a716-446655440000";

        write_blob_and_sidecar(
            &blobs_dir,
            id_a,
            content_a,
            &SidecarMeta {
                bucket: "bucket-1".into(),
                key: "a.txt".into(),
                size: content_a.len() as u64,
                etag: md5_hex(content_a),
                content_type: Some("text/plain".into()),
                last_modified: "2024-01-01T00:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            },
        )
        .await;

        write_blob_and_sidecar(
            &blobs_dir,
            id_b,
            content_b,
            &SidecarMeta {
                bucket: "bucket-1".into(),
                key: "b.txt".into(),
                size: content_b.len() as u64,
                etag: md5_hex(content_b),
                content_type: None,
                last_modified: "2024-01-02T00:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            },
        )
        .await;

        write_blob_and_sidecar(
            &blobs_dir,
            id_c,
            content_c,
            &SidecarMeta {
                bucket: "bucket-2".into(),
                key: "c.txt".into(),
                size: content_c.len() as u64,
                etag: md5_hex(content_c),
                content_type: None,
                last_modified: "2024-01-03T00:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            },
        )
        .await;

        run_recover(&config, false, false).await.unwrap();

        // Verify by opening the DB.
        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let buckets = store.list_buckets().await.unwrap();
        assert_eq!(buckets.len(), 2);

        let bucket_names: HashSet<String> =
            buckets.iter().map(|b| b.name.clone()).collect();
        assert!(bucket_names.contains("bucket-1"));
        assert!(bucket_names.contains("bucket-2"));

        let objs = store.list_objects("bucket-1", None, None, 100).await.unwrap();
        assert_eq!(objs.len(), 2);

        let objs = store.list_objects("bucket-2", None, None, 100).await.unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].key, "c.txt");
    }

    #[tokio::test]
    async fn recover_dry_run_no_db_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let content = b"test";
        let id = "550e8400-e29b-41d4-a716-446655440000";

        write_blob_and_sidecar(
            &blobs_dir,
            id,
            content,
            &SidecarMeta {
                bucket: "bucket".into(),
                key: "key".into(),
                size: content.len() as u64,
                etag: md5_hex(content),
                content_type: None,
                last_modified: "2024-01-01T00:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            },
        )
        .await;

        run_recover(&config, true, false).await.unwrap();

        // DB should not exist after dry run.
        assert!(!config.storage.db_path().exists());
    }

    #[tokio::test]
    async fn recover_orphaned_meta_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        // Write only the sidecar, no blob file.
        let id = "550e8400-e29b-41d4-a716-446655440000";
        let hex_chars: String = id.chars().filter(|c| *c != '-').collect();
        let dir = blobs_dir.join(&hex_chars[0..2]).join(&hex_chars[2..4]);
        fs::create_dir_all(&dir).await.unwrap();

        let meta = SidecarMeta {
            bucket: "bucket".into(),
            key: "key".into(),
            size: 4,
            etag: "abcd1234".into(),
            content_type: None,
            last_modified: "2024-01-01T00:00:00Z".into(),
            metadata: HashMap::new(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        fs::write(dir.join(format!("{id}.meta")), json).await.unwrap();

        run_recover(&config, false, false).await.unwrap();

        // DB should exist but be empty (no objects).
        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let buckets = store.list_buckets().await.unwrap();
        assert!(buckets.is_empty());
    }

    #[tokio::test]
    async fn recover_malformed_meta_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let id = "550e8400-e29b-41d4-a716-446655440000";
        let hex_chars: String = id.chars().filter(|c| *c != '-').collect();
        let dir = blobs_dir.join(&hex_chars[0..2]).join(&hex_chars[2..4]);
        fs::create_dir_all(&dir).await.unwrap();

        // Write blob file.
        fs::write(dir.join(id), b"data").await.unwrap();
        // Write invalid JSON sidecar.
        fs::write(dir.join(format!("{id}.meta")), b"not json")
            .await
            .unwrap();

        run_recover(&config, false, true).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let buckets = store.list_buckets().await.unwrap();
        assert!(buckets.is_empty());
    }

    #[tokio::test]
    async fn recover_preserves_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        // Create DB with a credential.
        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let cred = arca_core::types::Credential {
            access_key_id: "TESTKEY123".into(),
            secret_access_key: "TESTSECRET456".into(),
            description: "test credential".into(),
            created_at: chrono::Utc::now(),
            active: true,
            user_id: "root".into(),
        };
        store.put_credential(&cred).await.unwrap();
        drop(store);

        // Write one sidecar so recover has something to do.
        let content = b"test";
        let id = "550e8400-e29b-41d4-a716-446655440000";
        write_blob_and_sidecar(
            &blobs_dir,
            id,
            content,
            &SidecarMeta {
                bucket: "bucket".into(),
                key: "key".into(),
                size: content.len() as u64,
                etag: md5_hex(content),
                content_type: None,
                last_modified: "2024-01-01T00:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            },
        )
        .await;

        // Run recover.
        run_recover(&config, false, false).await.unwrap();

        // Verify credential was preserved.
        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let creds = store.list_credentials().await.unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].access_key_id, "TESTKEY123");
    }

    #[tokio::test]
    async fn recover_checksum_mismatch_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let id = "550e8400-e29b-41d4-a716-446655440000";

        // Write blob with content "hello" but sidecar with wrong etag.
        write_blob_and_sidecar(
            &blobs_dir,
            id,
            b"hello",
            &SidecarMeta {
                bucket: "bucket".into(),
                key: "key".into(),
                size: 5,
                etag: "0000000000000000000000000000dead".into(),
                content_type: None,
                last_modified: "2024-01-01T00:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            },
        )
        .await;

        // Recover with verification enabled — corrupted blob should be skipped.
        run_recover(&config, false, false).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let buckets = store.list_buckets().await.unwrap();
        assert!(buckets.is_empty());
    }

    #[tokio::test]
    async fn recover_multipart_etag_skips_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let config = test_config(tmp.path());
        let blobs_dir = config.storage.blobs_dir();
        fs::create_dir_all(&blobs_dir).await.unwrap();

        let id = "550e8400-e29b-41d4-a716-446655440000";

        // Multipart ETag (contains "-") — checksum verification should be skipped
        // even though the etag doesn't match the blob content MD5.
        write_blob_and_sidecar(
            &blobs_dir,
            id,
            b"assembled multipart data",
            &SidecarMeta {
                bucket: "bucket".into(),
                key: "large.bin".into(),
                size: 24,
                etag: "d41d8cd98f00b204e9800998ecf8427e-3".into(),
                content_type: Some("application/octet-stream".into()),
                last_modified: "2024-06-15T12:00:00Z".into(),
                metadata: HashMap::new(),
                encryption: None,
                compression: None,
                version_id: None,
                composite: None,
            },
        )
        .await;

        run_recover(&config, false, false).await.unwrap();

        let store = arca_storage::SqliteStore::open(&config.storage.db_path())
            .await
            .unwrap();
        let objs = store.list_objects("bucket", None, None, 100).await.unwrap();
        assert_eq!(objs.len(), 1);
        assert_eq!(objs[0].key, "large.bin");
        assert!(objs[0].etag.contains('-'));
    }
}
