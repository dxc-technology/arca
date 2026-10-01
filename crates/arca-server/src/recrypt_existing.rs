//! Offline `arca encrypt-existing` / `arca decrypt-existing`.
//!
//! These are the disaster-recovery escape hatch for the Phase 30 maintenance
//! re-encryption jobs. They run with the server STOPPED: each blob file is
//! transformed in place and the matching `.meta` sidecar and metadata-DB row are
//! updated to reflect the new encryption state. The blob and sidecar swap is
//! durable and crash-safe (fsynced temp file, then a journaled swap, see
//! [`arca_storage::inplace`]); an interrupted run is repaired when the tool is
//! run again, before it looks at the sidecar.
//!
//! Both directions reuse the shared re-crypt core in
//! [`arca_storage::recrypt`], so a blob encrypted here is byte-for-byte
//! identical to one written through the live encrypted path. The file-level
//! helpers are idempotent (they check the `AENC` magic), so re-running a job is
//! always safe.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arca_core::store::blob::SidecarMeta;
use arca_core::store::MetadataStore;
use arca_core::types::BlobId;
use std::sync::Arc;
use tokio::fs;

use crate::config::Config;
use crate::rewrite_marker;
use arca_storage::encryption::keys::MasterKey;
use arca_storage::inplace::{self, PendingOutcome};
use arca_storage::recrypt::{self, RecryptDirection};
use arca_storage::FsBlobStore;

/// Entry point for `arca encrypt-existing`: plaintext blobs → SSE-S3 (AES256).
pub async fn run_encrypt_existing(
    config: &Config,
    dry_run: bool,
    bucket_filter: Option<&str>,
    prefix_filter: Option<&str>,
) -> Result<()> {
    run(
        config,
        RecryptDirection::Encrypt,
        dry_run,
        bucket_filter,
        prefix_filter,
    )
    .await
}

/// Entry point for `arca decrypt-existing`: SSE-S3 (AES256) blobs → plaintext.
pub async fn run_decrypt_existing(
    config: &Config,
    dry_run: bool,
    bucket_filter: Option<&str>,
    prefix_filter: Option<&str>,
) -> Result<()> {
    run(
        config,
        RecryptDirection::Decrypt,
        dry_run,
        bucket_filter,
        prefix_filter,
    )
    .await
}

/// Shared driver for both directions.
async fn run(
    config: &Config,
    direction: RecryptDirection,
    dry_run: bool,
    bucket_filter: Option<&str>,
    prefix_filter: Option<&str>,
) -> Result<()> {
    let action = match direction {
        RecryptDirection::Encrypt => "encrypt",
        RecryptDirection::Decrypt => "decrypt",
    };

    let blobs_dir = config.storage.blobs_dir();
    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }

    // 1. Resolve the master key (fail fast if none is configured).
    let master_key = resolve_master_key(config)
        .await
        .context("resolving master key for re-encryption")?;

    // 2. Open the blob store and the metadata store (backend-agnostic).
    let fs_store = FsBlobStore::new(&blobs_dir, config.storage.blob_prefix_depth).await?;
    // The metadata row update is skipped in dry-run, so only open the DB when we
    // are actually going to write. This keeps a true dry-run side-effect free.
    let metadata: Option<Arc<dyn MetadataStore>> = if dry_run {
        None
    } else {
        Some(open_metadata_store(config).await?)
    };

    // Marker first (real runs only); removed below only if nothing failed.
    let data_dir = Path::new(&config.storage.data_dir);
    if !dry_run {
        let mut args = Vec::new();
        if let Some(b) = bucket_filter {
            args.extend(["--bucket".to_string(), b.to_string()]);
        }
        if let Some(p) = prefix_filter {
            args.extend(["--prefix".to_string(), p.to_string()]);
        }
        rewrite_marker::begin(data_dir, &format!("{action}-existing"), args)?;
    }

    // 3. Walk the sidecars.
    let meta_files = collect_meta_files(&blobs_dir).await?;
    println!("Found {} sidecar(s)", meta_files.len());

    let mut processed = 0u64;
    let mut skipped = 0u64;
    let mut errors = 0u64;

    for meta_path in meta_files {
        // Repair what an interrupted run left behind BEFORE reading the sidecar.
        if !dry_run {
            match inplace::resolve_pending(&meta_path).await {
                Ok(PendingOutcome::Clean) => {}
                Ok(PendingOutcome::Discarded) => println!(
                    "recovered {}: discarded an interrupted rewrite, blob and sidecar unchanged",
                    meta_path.display()
                ),
                Ok(PendingOutcome::RolledForward(done)) => {
                    println!(
                        "recovered {}: completed an interrupted rewrite",
                        meta_path.display()
                    );
                    let metadata = metadata.as_ref().expect("metadata store opened when not dry-run");
                    if let Err(e) = sync_row(metadata.as_ref(), &done).await {
                        eprintln!("WARNING: {}: {e:#}", meta_path.display());
                        errors += 1;
                    }
                }
                Err(e) => {
                    eprintln!("WARNING: {}: {e}", meta_path.display());
                    errors += 1;
                    continue;
                }
            }
        }
        let mut meta = match read_sidecar(&meta_path).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("WARNING: {}: {e}", meta_path.display());
                errors += 1;
                continue;
            }
        };

        // Bucket / prefix filters.
        if let Some(b) = bucket_filter {
            if meta.bucket != b {
                skipped += 1;
                continue;
            }
        }
        if let Some(p) = prefix_filter {
            if !meta.key.starts_with(p) {
                skipped += 1;
                continue;
            }
        }

        // Hard skips (composite / SSE-C / already-target) come from the shared
        // core so the offline and online surfaces agree exactly.
        if let Some(reason) = recrypt::classify_skip(&meta, direction) {
            println!(
                "skip {} ({}/{}): {}",
                meta_path.display(),
                meta.bucket,
                meta.key,
                reason.as_str()
            );
            skipped += 1;
            continue;
        }

        let blob_id = match blob_id_from_meta_path(&meta_path) {
            Ok(id) => id,
            Err(e) => {
                eprintln!("WARNING: {}: {e}", meta_path.display());
                errors += 1;
                continue;
            }
        };
        let blob_path = fs_store.blob_path(&blob_id);

        if dry_run {
            println!(
                "[dry-run] would {} {} ({}/{}, {} bytes)",
                action,
                blob_path.display(),
                meta.bucket,
                meta.key,
                meta.size
            );
            processed += 1;
            continue;
        }

        let metadata = metadata.as_ref().expect("metadata store opened when not dry-run");
        match direction {
            RecryptDirection::Encrypt => {
                match process_encrypt(&blob_path, &meta_path, &mut meta, &master_key, metadata.as_ref()).await {
                    Ok(true) => processed += 1,
                    Ok(false) => skipped += 1,
                    Err(e) => {
                        eprintln!(
                            "WARNING: {} ({}/{}): {e:#}",
                            blob_path.display(),
                            meta.bucket,
                            meta.key
                        );
                        errors += 1;
                    }
                }
            }
            RecryptDirection::Decrypt => {
                match process_decrypt(&blob_path, &meta_path, &mut meta, &master_key, metadata.as_ref()).await {
                    Ok(true) => processed += 1,
                    Ok(false) => skipped += 1,
                    Err(e) => {
                        eprintln!(
                            "WARNING: {} ({}/{}): {e:#}",
                            blob_path.display(),
                            meta.bucket,
                            meta.key
                        );
                        errors += 1;
                    }
                }
            }
        }
    }

    let incomplete = !dry_run && errors > 0;
    if !dry_run && !incomplete {
        rewrite_marker::finish(data_dir)?;
    }

    println!(
        "\nDone. {action}ed={processed} skipped={skipped} errors={errors}"
    );

    // The marker stays and `arca serve` stays blocked: scripts must see it too.
    if incomplete {
        let mut rerun = format!("arca {action}-existing");
        if let Some(b) = bucket_filter {
            rerun.push_str(&format!(" --bucket {b}"));
        }
        if let Some(p) = prefix_filter {
            rerun.push_str(&format!(" --prefix {p}"));
        }
        anyhow::bail!(
            "{errors} object(s) failed; the rewrite is incomplete and `arca serve` stays blocked. \
             Fix the cause and re-run `{rerun}`"
        );
    }
    Ok(())
}

/// Encrypts one blob in place, rewrites its sidecar, and sets the DB row's
/// encryption columns. Returns `Ok(true)` if it was encrypted, `Ok(false)` if
/// the file was already encrypted (idempotent no-op).
async fn process_encrypt(
    blob_path: &Path,
    meta_path: &Path,
    meta: &mut SidecarMeta,
    master_key: &MasterKey,
    metadata: &dyn MetadataStore,
) -> Result<bool> {
    let prepared = recrypt::prepare_encrypt_file(blob_path, master_key)
        .await
        .with_context(|| format!("encrypting {}", blob_path.display()))?;
    let prepared = match prepared {
        Some(p) => p,
        // Already an AENC blob on disk — nothing to do.
        None => return Ok(false),
    };

    // Blob and sidecar swap together (the sidecar holds the only copy of the
    // wrapped DEK), then the DB row, the same order as the live path.
    meta.encryption = Some(prepared.outcome.encryption);
    commit(blob_path, meta_path, meta).await?;
    sync_row(metadata, meta).await?;
    Ok(true)
}

/// Decrypts one blob in place, clears its sidecar encryption, and clears the DB
/// row's encryption columns. Returns `Ok(true)` if it was decrypted, `Ok(false)`
/// if the file was already plaintext (idempotent no-op).
async fn process_decrypt(
    blob_path: &Path,
    meta_path: &Path,
    meta: &mut SidecarMeta,
    master_key: &MasterKey,
    metadata: &dyn MetadataStore,
) -> Result<bool> {
    // classify_skip already guaranteed an AES256 sidecar, but guard anyway.
    let enc_info = meta
        .encryption
        .clone()
        .context("sidecar has no encryption metadata to decrypt")?;

    let prepared = recrypt::prepare_decrypt_file(blob_path, &enc_info, master_key)
        .await
        .with_context(|| format!("decrypting {}", blob_path.display()))?;
    if prepared.is_none() {
        // File was already plaintext on disk.
        return Ok(false);
    }

    meta.encryption = None;
    commit(blob_path, meta_path, meta).await?;
    sync_row(metadata, meta).await?;
    Ok(true)
}

/// Publishes the prepared temp blob and the new sidecar (durable, journaled).
async fn commit(blob_path: &Path, meta_path: &Path, meta: &SidecarMeta) -> Result<()> {
    inplace::commit_rewrite(blob_path, meta_path, meta)
        .await
        .with_context(|| format!("rewriting {} and its sidecar", blob_path.display()))
}

/// Makes the object row's encryption columns match the sidecar.
async fn sync_row(metadata: &dyn MetadataStore, meta: &SidecarMeta) -> Result<()> {
    let (algorithm, key_id) = match &meta.encryption {
        Some(e) => (Some(recrypt::AES256), Some(e.key_id.as_str())),
        None => (None, None),
    };
    metadata
        .update_object_encryption(&meta.bucket, &meta.key, meta.version_id.as_deref(), algorithm, key_id)
        .await
        .with_context(|| format!("updating row {}/{}", meta.bucket, meta.key))?;
    Ok(())
}

/// Resolves the master key from config exactly as the `serve` path does:
/// either from KMS (Vault/OpenBAO) or from `[encryption].master_key`. Fails
/// fast with a clear message when no key is configured.
async fn resolve_master_key(config: &Config) -> Result<MasterKey> {
    match &config.encryption {
        Some(enc) if enc.kms.is_some() => {
            let kms = enc.kms.as_ref().unwrap();
            crate::vault::ensure_master_key(kms)
                .await
                .context("fetching master key from KMS")
        }
        Some(enc) if enc.master_key.is_some() => {
            MasterKey::from_base64(enc.master_key.as_ref().unwrap())
                .map_err(|e| anyhow::anyhow!("invalid master key: {e}"))
        }
        _ => anyhow::bail!(
            "no master key configured: set [encryption].master_key or [encryption.kms] in the config"
        ),
    }
}

/// Opens the metadata store for the configured backend (sqlite or postgres),
/// reusing the same selection logic as the server's `open_stores`.
async fn open_metadata_store(config: &Config) -> Result<Arc<dyn MetadataStore>> {
    match config.storage.metadata_backend.as_str() {
        "sqlite" => {
            let store = arca_storage::SqliteStore::open(&config.storage.db_path()).await?;
            Ok(Arc::new(store) as Arc<dyn MetadataStore>)
        }
        "postgres" => {
            let pg = config.storage.postgres.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "[storage.postgres] section required when metadata_backend = \"postgres\""
                )
            })?;
            let store =
                arca_storage::PgStore::open(&pg.connection_string, pg.max_connections).await?;
            Ok(Arc::new(store) as Arc<dyn MetadataStore>)
        }
        other => anyhow::bail!("Unknown metadata backend: \"{other}\""),
    }
}

async fn collect_meta_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut rd = fs::read_dir(&d)
            .await
            .with_context(|| format!("reading {}", d.display()))?;
        while let Some(entry) = rd.next_entry().await? {
            let p = entry.path();
            if entry.file_type().await?.is_dir() {
                stack.push(p);
            } else if let Some(n) = p.file_name().and_then(|n| n.to_str()) {
                if n.ends_with(".meta") {
                    out.push(p);
                }
            }
        }
    }
    Ok(out)
}

async fn read_sidecar(path: &Path) -> Result<SidecarMeta> {
    let json = fs::read_to_string(path).await?;
    let meta: SidecarMeta = serde_json::from_str(&json)?;
    Ok(meta)
}

fn blob_id_from_meta_path(meta_path: &Path) -> Result<BlobId> {
    let name = meta_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid sidecar filename")?;
    let id = name
        .strip_suffix(".meta")
        .context("sidecar does not end with .meta")?;
    Ok(BlobId(id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::BlobStore;
    use tokio_stream::StreamExt;

    const KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    const ID: &str = "aabbccdd-0000-4000-8000-000000000001";

    fn test_config(data_dir: &Path) -> Config {
        toml::from_str(&format!(
            "[server]\nbind = \"127.0.0.1\"\nport = 9000\n\n[storage]\ndata_dir = \"{}\"\n\n[encryption]\nmaster_key = \"{KEY}\"\n",
            data_dir.display()
        ))
        .unwrap()
    }

    fn payload() -> Vec<u8> {
        b"re-encryption payload\n".repeat(500)
    }

    fn meta(size: u64) -> SidecarMeta {
        SidecarMeta {
            bucket: "b".into(),
            key: "k".into(),
            size,
            etag: "e".into(),
            content_type: None,
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: Default::default(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    /// Plain blob + sidecar under `<tmp>/blobs`. Returns (blob, sidecar).
    async fn fixture(tmp: &Path, body: &[u8]) -> (PathBuf, PathBuf) {
        let store = FsBlobStore::new(&tmp.join("blobs"), 2).await.unwrap();
        let blob = store.blob_path(&BlobId(ID.to_string()));
        fs::create_dir_all(blob.parent().unwrap()).await.unwrap();
        fs::write(&blob, body).await.unwrap();
        let mp = PathBuf::from(format!("{}.meta", blob.display()));
        fs::write(&mp, serde_json::to_string(&meta(body.len() as u64)).unwrap())
            .await
            .unwrap();
        (blob, mp)
    }

    /// Reads the blob back through the real encrypting store.
    async fn read_back(tmp: &Path) -> Vec<u8> {
        let fs_store = FsBlobStore::new(&tmp.join("blobs"), 2).await.unwrap();
        let mk = Arc::new(MasterKey::from_base64(KEY).unwrap());
        let store = arca_storage::EncryptingBlobStore::new(fs_store, mk);
        let mut stream = store.get(&BlobId(ID.to_string()), None).await.unwrap().stream;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    fn dir_names(blob: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(blob.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn encrypt_then_decrypt_read_back_and_leave_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let (blob, mp) = fixture(tmp.path(), &body).await;
        let config = test_config(tmp.path());
        let expected = vec![ID.to_string(), format!("{ID}.meta")];

        run_encrypt_existing(&config, false, None, None).await.unwrap();
        assert!(read_sidecar(&mp).await.unwrap().encryption.is_some());
        assert_eq!(&fs::read(&blob).await.unwrap()[..4], b"AENC");
        assert_eq!(read_back(tmp.path()).await, body);
        assert_eq!(dir_names(&blob), expected);

        run_decrypt_existing(&config, false, None, None).await.unwrap();
        assert!(read_sidecar(&mp).await.unwrap().encryption.is_none());
        assert_eq!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(dir_names(&blob), expected);
    }

    #[tokio::test]
    async fn failed_rewrite_keeps_original_blob_and_sidecar() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let (blob, mp) = fixture(tmp.path(), &body).await;
        let sidecar_before = fs::read(&mp).await.unwrap();
        let pending = inplace::pending_sidecar_path(&mp);
        fs::create_dir(&pending).await.unwrap();
        fs::write(pending.join("keep"), b"x").await.unwrap();

        // Per-blob errors are reported and make the run fail (non-zero exit).
        let err = run_encrypt_existing(&test_config(tmp.path()), false, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("1 object(s) failed"), "{err}");
        assert!(err.contains("arca encrypt-existing"), "{err}");

        assert_eq!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(fs::read(&mp).await.unwrap(), sidecar_before);
        assert!(!inplace::rewrite_tmp_path(&blob).exists(), "temp blob must be removed");
    }

    #[tokio::test]
    async fn crash_between_blob_and_sidecar_rename_keeps_the_dek() {
        // The encrypted blob is in place and only the journal knows the DEK.
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let (blob, mp) = fixture(tmp.path(), &body).await;
        let mk = MasterKey::from_base64(KEY).unwrap();
        let prepared = recrypt::prepare_encrypt_file(&blob, &mk).await.unwrap().unwrap();
        let mut new = read_sidecar(&mp).await.unwrap();
        new.encryption = Some(prepared.outcome.encryption);
        fs::write(inplace::pending_sidecar_path(&mp), serde_json::to_string(&new).unwrap())
            .await
            .unwrap();
        fs::rename(&prepared.tmp, &blob).await.unwrap();
        assert!(read_sidecar(&mp).await.unwrap().encryption.is_none());

        run_encrypt_existing(&test_config(tmp.path()), false, None, None)
            .await
            .unwrap();

        assert!(read_sidecar(&mp).await.unwrap().encryption.is_some());
        assert_eq!(read_back(tmp.path()).await, body);
        assert_eq!(dir_names(&blob), vec![ID.to_string(), format!("{ID}.meta")]);
    }

    #[tokio::test]
    async fn crash_before_blob_rename_is_discarded_and_rerun_encrypts() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let (blob, mp) = fixture(tmp.path(), &body).await;
        let mk = MasterKey::from_base64(KEY).unwrap();
        let prepared = recrypt::prepare_encrypt_file(&blob, &mk).await.unwrap().unwrap();
        let mut new = read_sidecar(&mp).await.unwrap();
        new.encryption = Some(prepared.outcome.encryption);
        fs::write(inplace::pending_sidecar_path(&mp), serde_json::to_string(&new).unwrap())
            .await
            .unwrap();
        assert_eq!(fs::read(&blob).await.unwrap(), body);

        run_encrypt_existing(&test_config(tmp.path()), false, None, None)
            .await
            .unwrap();

        assert_eq!(read_back(tmp.path()).await, body);
        assert_eq!(dir_names(&blob), vec![ID.to_string(), format!("{ID}.meta")]);
    }

    fn marker_path(tmp: &Path) -> PathBuf {
        crate::rewrite_marker::marker_path(tmp)
    }

    #[tokio::test]
    async fn marker_not_written_in_dry_run_and_removed_after_clean_run() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path(), &payload()).await;
        let config = test_config(tmp.path());

        run_encrypt_existing(&config, true, None, None).await.unwrap();
        assert!(!marker_path(tmp.path()).exists());

        // A marker seeded directly (independent of `begin`) must be removed by
        // `finish` after a clean run, and left alone by a dry run.
        let marker = crate::rewrite_marker::Marker {
            command: "encrypt-existing".into(),
            args: vec![],
            started_at: "2026-01-01T00:00:00Z".into(),
            pid: 1,
        };
        std::fs::write(marker_path(tmp.path()), serde_json::to_vec(&marker).unwrap()).unwrap();
        run_encrypt_existing(&config, true, None, None).await.unwrap();
        assert!(marker_path(tmp.path()).exists());

        run_encrypt_existing(&config, false, None, None).await.unwrap();
        assert!(!marker_path(tmp.path()).exists());
    }

    #[tokio::test]
    async fn pending_marker_of_another_command_refuses_and_same_command_clears_it() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path(), &payload()).await;
        let config = test_config(tmp.path());
        crate::rewrite_marker::begin(tmp.path(), "encrypt-existing", vec!["--bucket".into(), "b".into()])
            .unwrap();

        let err = run_decrypt_existing(&config, false, None, None).await.unwrap_err().to_string();
        assert!(err.contains("arca encrypt-existing --bucket b"), "{err}");
        assert!(marker_path(tmp.path()).exists());

        run_encrypt_existing(&config, false, None, None).await.unwrap();
        assert!(!marker_path(tmp.path()).exists());
    }

    #[tokio::test]
    async fn marker_kept_when_a_blob_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (_blob, mp) = fixture(tmp.path(), &payload()).await;
        // Corrupt sidecar: counted as an error, the run is not clean.
        fs::write(&mp, "not json").await.unwrap();
        let err = run_encrypt_existing(&test_config(tmp.path()), false, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("1 object(s) failed"), "{err}");
        assert!(marker_path(tmp.path()).exists());
    }

    #[tokio::test]
    async fn decrypt_with_a_failing_blob_errors_and_keeps_the_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let (_blob, mp) = fixture(tmp.path(), &payload()).await;
        fs::write(&mp, "not json").await.unwrap();
        let err = run_decrypt_existing(&test_config(tmp.path()), false, None, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("arca decrypt-existing"), "{err}");
        assert!(marker_path(tmp.path()).exists());
    }

    #[tokio::test]
    async fn dry_run_with_a_failing_blob_still_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let (_blob, mp) = fixture(tmp.path(), &payload()).await;
        fs::write(&mp, "not json").await.unwrap();
        run_encrypt_existing(&test_config(tmp.path()), true, None, None).await.unwrap();
        assert!(!marker_path(tmp.path()).exists());
    }
}
