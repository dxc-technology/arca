//! Offline `arca encrypt-existing` / `arca decrypt-existing`.
//!
//! These are the disaster-recovery escape hatch for the Phase 30 maintenance
//! re-encryption jobs. They run with the server STOPPED: each blob file is
//! transformed in place (atomic temp + rename) and the matching `.meta` sidecar
//! and metadata-DB row are updated to reflect the new encryption state.
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
use arca_storage::encryption::keys::MasterKey;
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

    // 3. Walk the sidecars.
    let meta_files = collect_meta_files(&blobs_dir).await?;
    println!("Found {} sidecar(s)", meta_files.len());

    let mut processed = 0u64;
    let mut skipped = 0u64;
    let mut errors = 0u64;

    for meta_path in meta_files {
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

    println!(
        "\nDone. {action}ed={processed} skipped={skipped} errors={errors}"
    );
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
    let outcome = recrypt::encrypt_file_in_place(blob_path, master_key)
        .await
        .with_context(|| format!("encrypting {}", blob_path.display()))?;
    let outcome = match outcome {
        Some(o) => o,
        // Already an AENC blob on disk — nothing to do.
        None => return Ok(false),
    };

    // Sidecar first, then the DB row (same write order as the live path).
    meta.encryption = Some(outcome.encryption.clone());
    write_sidecar(meta_path, meta)
        .await
        .with_context(|| format!("rewriting sidecar {}", meta_path.display()))?;

    metadata
        .update_object_encryption(
            &meta.bucket,
            &meta.key,
            meta.version_id.as_deref(),
            Some(recrypt::AES256),
            Some(&outcome.encryption.key_id),
        )
        .await
        .with_context(|| format!("updating row {}/{}", meta.bucket, meta.key))?;
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

    let did = recrypt::decrypt_file_in_place(blob_path, &enc_info, master_key)
        .await
        .with_context(|| format!("decrypting {}", blob_path.display()))?;
    if !did {
        // File was already plaintext on disk.
        return Ok(false);
    }

    meta.encryption = None;
    write_sidecar(meta_path, meta)
        .await
        .with_context(|| format!("rewriting sidecar {}", meta_path.display()))?;

    metadata
        .update_object_encryption(
            &meta.bucket,
            &meta.key,
            meta.version_id.as_deref(),
            None,
            None,
        )
        .await
        .with_context(|| format!("updating row {}/{}", meta.bucket, meta.key))?;
    Ok(true)
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

async fn write_sidecar(path: &Path, meta: &SidecarMeta) -> Result<()> {
    let tmp = path.with_extension("meta.tmp");
    let json = serde_json::to_string(meta)?;
    fs::write(&tmp, json).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
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
