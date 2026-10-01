//! Offline `arca compress-existing` / `arca decompress-existing`.
//!
//! Walks the blobs directory, reads each `.meta` sidecar, and either
//! compresses or decompresses the associated blob file in place. The new bytes
//! go to an fsynced temp file, then blob and sidecar are swapped together by
//! [`arca_storage::inplace::commit_rewrite`]: durable, and repairable when the
//! tool is run again after a crash (see that module for the protocol).

use std::path::{Path, PathBuf};
use std::pin::Pin;

use anyhow::{Context, Result};
use arca_core::store::blob::{BlobCompressionInfo, SidecarMeta};
use arca_core::store::CompressionAlgorithm;
use arca_core::types::BlobId;
use bytes::Bytes;
use futures_core::Stream;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use crate::config::Config;
use crate::rewrite_marker;
use arca_storage::compression::auto::pick_auto;
use arca_storage::compression::format::{self, FOOTER_COUNT_SIZE, HEADER_SIZE};
use arca_storage::compression::stream::{CompressingStream, DecompressingStream};
use arca_storage::inplace::{self, PendingOutcome};
use arca_storage::FsBlobStore;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

/// Why the offline tools must leave a blob alone, if they must. Encrypted
/// blobs cannot be touched without the key (compressing the ciphertext would
/// make the object unreadable: the live order is compress, then encrypt), and
/// composite (multipart) blobs have no file of their own.
fn skip_reason(meta: &SidecarMeta) -> Option<&'static str> {
    if meta.composite.is_some() {
        Some("composite (multipart) blob has no file of its own")
    } else if let Some(enc) = &meta.encryption {
        if enc.algorithm == "SSE-C" {
            Some("SSE-C encrypted blob, the key is held by the client")
        } else {
            Some("encrypted blob, this offline tool cannot rewrite it without going through the encryption layer")
        }
    } else {
        None
    }
}

pub async fn run_compress_existing(
    config: &Config,
    dry_run: bool,
    bucket_filter: Option<&str>,
    algorithm_override: Option<&str>,
) -> Result<()> {
    let blobs_dir = config.storage.blobs_dir();
    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }

    // Resolve the algorithm for this batch: either the explicit override or
    // `auto` (same default as the console picker and the live path).
    let algo_name = algorithm_override.unwrap_or("auto").to_string();
    let default_level: i32 = 3;

    let meta_files = collect_meta_files(&blobs_dir).await?;
    println!("Found {} sidecar(s)", meta_files.len());

    let fs_store = FsBlobStore::new(&blobs_dir, config.storage.blob_prefix_depth).await?;

    // Marker first (real runs only); removed below once every candidate went
    // through. Any `?` in between leaves it in place.
    let data_dir = Path::new(&config.storage.data_dir);
    if !dry_run {
        let mut args = filter_args(bucket_filter);
        if let Some(a) = algorithm_override {
            args.extend(["--algorithm".to_string(), a.to_string()]);
        }
        rewrite_marker::begin(data_dir, "compress-existing", args)?;
    }

    let mut compressed = 0u64;
    let mut skipped = 0u64;
    let mut ratio_plain: u64 = 0;
    let mut ratio_comp: u64 = 0;

    for meta_path in meta_files {
        // Repair what an interrupted run left behind BEFORE reading the sidecar.
        if !dry_run {
            resolve_interrupted(&meta_path).await?;
        }
        let mut meta = match read_sidecar(&meta_path).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("WARNING: {}: {}", meta_path.display(), e);
                skipped += 1;
                continue;
            }
        };

        if meta.compression.is_some() {
            skipped += 1;
            continue;
        }
        if let Some(b) = bucket_filter {
            if meta.bucket != b {
                skipped += 1;
                continue;
            }
        }
        if let Some(reason) = skip_reason(&meta) {
            eprintln!("SKIP {}: {}", meta_path.display(), reason);
            skipped += 1;
            continue;
        }
        // Same MIME/size filters as the live path.
        if meta.size < arca_storage::compressed_blob::DEFAULT_MIN_SIZE {
            skipped += 1;
            continue;
        }
        if let Some(ct) = meta.content_type.as_deref() {
            let ct_lower = ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase();
            if arca_storage::compressed_blob::DEFAULT_SKIP_MIME_PREFIXES
                .iter()
                .any(|p| ct_lower.starts_with(p))
            {
                skipped += 1;
                continue;
            }
        }

        let (algorithm, level) = if algo_name == "auto" {
            pick_auto(meta.content_type.as_deref(), Some(meta.size))
        } else {
            let alg = CompressionAlgorithm::parse(&algo_name)
                .with_context(|| format!("unknown compression algorithm: {algo_name}"))?;
            (alg, default_level)
        };

        let blob_id = blob_id_from_meta_path(&meta_path)?;
        let blob_path = fs_store.blob_path(&blob_id);

        if dry_run {
            println!(
                "[dry-run] would compress {} ({} bytes) with {} level {}",
                blob_path.display(),
                meta.size,
                algorithm.as_str(),
                level
            );
            compressed += 1;
            continue;
        }

        let info = compress_blob_to_tmp(
            &blob_path,
            algorithm,
            level,
            arca_storage::compressed_blob::DEFAULT_CHUNK_SIZE,
            meta.size,
        )
        .await
        .with_context(|| format!("compressing {}", blob_path.display()))?;

        meta.compression = Some(info.clone());
        inplace::commit_rewrite(&blob_path, &meta_path, &meta)
            .await
            .with_context(|| format!("rewriting {} and its sidecar", blob_path.display()))?;

        ratio_plain += info.original_size;
        ratio_comp += info.compressed_size;
        compressed += 1;
    }

    if !dry_run {
        rewrite_marker::finish(data_dir)?;
    }

    let ratio = if ratio_comp > 0 {
        ratio_plain as f64 / ratio_comp as f64
    } else {
        0.0
    };
    println!(
        "\nDone. compressed={} skipped={} plaintext={}B compressed={}B ratio={:.3}",
        compressed, skipped, ratio_plain, ratio_comp, ratio
    );
    Ok(())
}

pub async fn run_decompress_existing(
    config: &Config,
    dry_run: bool,
    bucket_filter: Option<&str>,
) -> Result<()> {
    let blobs_dir = config.storage.blobs_dir();
    if !blobs_dir.exists() {
        anyhow::bail!("Blobs directory does not exist: {}", blobs_dir.display());
    }
    let meta_files = collect_meta_files(&blobs_dir).await?;
    let fs_store = FsBlobStore::new(&blobs_dir, config.storage.blob_prefix_depth).await?;

    let data_dir = Path::new(&config.storage.data_dir);
    if !dry_run {
        rewrite_marker::begin(data_dir, "decompress-existing", filter_args(bucket_filter))?;
    }

    let mut decompressed = 0u64;
    let mut skipped = 0u64;

    for meta_path in meta_files {
        // Repair what an interrupted run left behind BEFORE reading the sidecar.
        if !dry_run {
            resolve_interrupted(&meta_path).await?;
        }
        let mut meta = match read_sidecar(&meta_path).await {
            Ok(m) => m,
            Err(e) => {
                eprintln!("WARNING: {}: {}", meta_path.display(), e);
                skipped += 1;
                continue;
            }
        };
        if meta.compression.is_none() {
            skipped += 1;
            continue;
        }
        if let Some(b) = bucket_filter {
            if meta.bucket != b {
                skipped += 1;
                continue;
            }
        }
        if let Some(reason) = skip_reason(&meta) {
            eprintln!("SKIP {}: {}", meta_path.display(), reason);
            skipped += 1;
            continue;
        }
        let info = meta.compression.clone().unwrap();
        let blob_id = blob_id_from_meta_path(&meta_path)?;
        let blob_path = fs_store.blob_path(&blob_id);

        if dry_run {
            println!(
                "[dry-run] would decompress {} ({} → {} bytes, {})",
                blob_path.display(),
                info.compressed_size,
                info.original_size,
                info.algorithm.as_str()
            );
            decompressed += 1;
            continue;
        }

        decompress_blob_to_tmp(&blob_path, &info)
            .await
            .with_context(|| format!("decompressing {}", blob_path.display()))?;

        meta.compression = None;
        inplace::commit_rewrite(&blob_path, &meta_path, &meta)
            .await
            .with_context(|| format!("rewriting {} and its sidecar", blob_path.display()))?;
        decompressed += 1;
    }

    if !dry_run {
        rewrite_marker::finish(data_dir)?;
    }

    println!(
        "\nDone. decompressed={} skipped={}",
        decompressed, skipped
    );
    Ok(())
}

/// `--bucket` argument of a re-run, as recorded in the marker.
fn filter_args(bucket_filter: Option<&str>) -> Vec<String> {
    bucket_filter
        .map(|b| vec!["--bucket".to_string(), b.to_string()])
        .unwrap_or_default()
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

/// Repairs the leftovers of an interrupted run for one sidecar and says so.
async fn resolve_interrupted(meta_path: &Path) -> Result<()> {
    match inplace::resolve_pending(meta_path)
        .await
        .with_context(|| format!("repairing {}", meta_path.display()))?
    {
        PendingOutcome::Clean => {}
        PendingOutcome::Discarded => println!(
            "recovered {}: discarded an interrupted rewrite, blob and sidecar unchanged",
            meta_path.display()
        ),
        PendingOutcome::RolledForward(_) => println!(
            "recovered {}: completed an interrupted rewrite",
            meta_path.display()
        ),
    }
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

/// Reads the plaintext and writes the compressed bytes to the rewrite temp
/// file, fsynced. The blob is untouched until `commit_rewrite`.
async fn compress_blob_to_tmp(
    blob_path: &Path,
    algorithm: CompressionAlgorithm,
    level: i32,
    chunk_size: u32,
    plaintext_size: u64,
) -> Result<BlobCompressionInfo> {
    let tmp = inplace::rewrite_tmp_path(blob_path);
    let file = fs::File::open(blob_path).await?;
    let reader: ByteStream = Box::pin(ReaderStream::new(file));
    let (comp_stream, _stats) = CompressingStream::new(reader, algorithm, level, chunk_size);

    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await?;
    let mut stream = std::pin::pin!(comp_stream);
    use tokio_stream::StreamExt;
    while let Some(chunk) = stream.as_mut().next().await {
        let chunk = chunk?;
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    out.sync_all().await?;
    drop(out);

    let compressed_size = fs::metadata(&tmp).await?.len();
    Ok(BlobCompressionInfo {
        algorithm,
        chunk_size,
        original_size: plaintext_size,
        compressed_size,
    })
}

/// Reads the compressed blob and writes the plaintext to the rewrite temp
/// file, fsynced. The blob is untouched until `commit_rewrite`.
async fn decompress_blob_to_tmp(
    blob_path: &Path,
    info: &BlobCompressionInfo,
) -> Result<()> {
    // Read footer to know data region bounds.
    let mut file = fs::File::open(blob_path).await?;
    let file_size = info.compressed_size;

    // Verify the header matches expectations.
    let mut header = [0u8; HEADER_SIZE];
    file.read_exact(&mut header).await?;
    let (algo_on_disk, _cs) = format::parse_header(&header)
        .map_err(|e| anyhow::anyhow!("invalid compression header: {e}"))?;
    if algo_on_disk != info.algorithm {
        anyhow::bail!(
            "sidecar says {:?}, file header says {:?}",
            info.algorithm,
            algo_on_disk
        );
    }

    // Load footer: last FOOTER_COUNT_SIZE bytes = n, preceded by n * 4 bytes.
    let count_off = file_size - FOOTER_COUNT_SIZE as u64;
    let mut count_buf = [0u8; FOOTER_COUNT_SIZE];
    tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(count_off)).await?;
    file.read_exact(&mut count_buf).await?;
    let n = u32::from_le_bytes(count_buf) as u64;
    let index_bytes = n * 4;
    let data_end = count_off - index_bytes;

    // Stream the data region through DecompressingStream.
    tokio::io::AsyncSeekExt::seek(&mut file, std::io::SeekFrom::Start(HEADER_SIZE as u64)).await?;
    let data_len = data_end - HEADER_SIZE as u64;
    let limited = file.take(data_len);
    let reader: ByteStream = Box::pin(ReaderStream::new(limited));
    let dec = DecompressingStream::new(reader, info.algorithm);

    let tmp = inplace::rewrite_tmp_path(blob_path);
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await?;
    let mut stream = std::pin::pin!(dec);
    use tokio_stream::StreamExt;
    while let Some(chunk) = stream.as_mut().next().await {
        let chunk = chunk?;
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    out.sync_all().await?;
    drop(out);

    let new_size = fs::metadata(&tmp).await?.len();
    if new_size != info.original_size {
        let _ = fs::remove_file(&tmp).await;
        anyhow::bail!(
            "decompressed size {} != sidecar original_size {}",
            new_size,
            info.original_size
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::store::blob::{BlobEncryptionInfo, CompositePart};
    use std::sync::Arc;

    fn test_config(data_dir: &Path) -> Config {
        toml::from_str(&format!(
            "[server]\nbind = \"127.0.0.1\"\nport = 9000\n\n[storage]\ndata_dir = \"{}\"\n",
            data_dir.display()
        ))
        .unwrap()
    }

    fn payload() -> Vec<u8> {
        b"compressible text line\n".repeat(400)
    }

    fn meta(size: u64) -> SidecarMeta {
        SidecarMeta {
            bucket: "b".into(),
            key: "k".into(),
            size,
            etag: "e".into(),
            content_type: Some("text/plain".into()),
            last_modified: "2026-01-01T00:00:00Z".into(),
            metadata: Default::default(),
            encryption: None,
            compression: None,
            version_id: None,
            composite: None,
        }
    }

    fn enc(algorithm: &str) -> BlobEncryptionInfo {
        BlobEncryptionInfo {
            algorithm: algorithm.into(),
            encrypted_dek: String::new(),
            dek_nonce: String::new(),
            nonce_prefix: "AAAAAA==".into(),
            key_id: String::new(),
        }
    }

    /// Writes a sidecar (and optionally a blob file) under `<tmp>/blobs`.
    /// Returns (blob path, sidecar path).
    async fn fixture(
        tmp: &Path,
        id: &str,
        meta: &SidecarMeta,
        body: Option<&[u8]>,
    ) -> (PathBuf, PathBuf) {
        let store = FsBlobStore::new(&tmp.join("blobs"), 2).await.unwrap();
        let blob_path = store.blob_path(&BlobId(id.to_string()));
        fs::create_dir_all(blob_path.parent().unwrap()).await.unwrap();
        if let Some(b) = body {
            fs::write(&blob_path, b).await.unwrap();
        }
        let meta_path = PathBuf::from(format!("{}.meta", blob_path.display()));
        fs::write(&meta_path, serde_json::to_string(meta).unwrap())
            .await
            .unwrap();
        (blob_path, meta_path)
    }

    const ID1: &str = "aabbccdd-0000-4000-8000-000000000001";
    const ID2: &str = "aabbccdd-0000-4000-8000-000000000002";

    #[tokio::test]
    async fn compress_skips_sse_s3_blob_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let mut m = meta(body.len() as u64);
        m.encryption = Some(enc("AES256"));
        let (blob, mp) = fixture(tmp.path(), ID1, &m, Some(&body)).await;
        let sidecar_before = fs::read(&mp).await.unwrap();

        run_compress_existing(&test_config(tmp.path()), false, None, Some("zstd"))
            .await
            .unwrap();

        assert_eq!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(fs::read(&mp).await.unwrap(), sidecar_before);
    }

    #[tokio::test]
    async fn compress_skips_sse_c_blob_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let mut m = meta(body.len() as u64);
        m.encryption = Some(enc("SSE-C"));
        let (blob, mp) = fixture(tmp.path(), ID1, &m, Some(&body)).await;
        let sidecar_before = fs::read(&mp).await.unwrap();

        run_compress_existing(&test_config(tmp.path()), false, None, Some("zstd"))
            .await
            .unwrap();

        assert_eq!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(fs::read(&mp).await.unwrap(), sidecar_before);
    }

    #[tokio::test]
    async fn compress_skips_composite_and_continues_with_plain_blob() {
        let tmp = tempfile::tempdir().unwrap();
        let mut comp = meta(4096);
        comp.composite = Some(vec![CompositePart {
            blob_id: BlobId("part-1".into()),
            plaintext_size: 4096,
            plaintext_etag: "e".into(),
            encryption: None,
        }]);
        let (comp_blob, comp_mp) = fixture(tmp.path(), ID1, &comp, None).await;
        let body = payload();
        let plain = meta(body.len() as u64);
        let (plain_blob, plain_mp) = fixture(tmp.path(), ID2, &plain, Some(&body)).await;
        let comp_sidecar = fs::read(&comp_mp).await.unwrap();

        run_compress_existing(&test_config(tmp.path()), false, None, Some("zstd"))
            .await
            .unwrap();

        assert!(!comp_blob.exists());
        assert_eq!(fs::read(&comp_mp).await.unwrap(), comp_sidecar);
        let after = read_sidecar(&plain_mp).await.unwrap();
        assert!(after.compression.is_some());
        assert_ne!(fs::read(&plain_blob).await.unwrap(), body);
    }

    #[tokio::test]
    async fn decompress_skips_composite_and_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let plain = meta(body.len() as u64);
        let (plain_blob, plain_mp) = fixture(tmp.path(), ID2, &plain, Some(&body)).await;
        let config = test_config(tmp.path());
        run_compress_existing(&config, false, None, Some("zstd"))
            .await
            .unwrap();

        let mut comp = meta(4096);
        comp.compression = Some(BlobCompressionInfo {
            algorithm: CompressionAlgorithm::Zstd,
            chunk_size: 1024,
            original_size: 4096,
            compressed_size: 100,
        });
        comp.composite = Some(vec![CompositePart {
            blob_id: BlobId("part-1".into()),
            plaintext_size: 4096,
            plaintext_etag: "e".into(),
            encryption: None,
        }]);
        let (_cb, comp_mp) = fixture(tmp.path(), ID1, &comp, None).await;
        let comp_sidecar = fs::read(&comp_mp).await.unwrap();

        run_decompress_existing(&config, false, None).await.unwrap();

        assert_eq!(fs::read(&comp_mp).await.unwrap(), comp_sidecar);
        assert_eq!(fs::read(&plain_blob).await.unwrap(), body);
        assert!(read_sidecar(&plain_mp).await.unwrap().compression.is_none());
    }

    #[tokio::test]
    async fn decompress_skips_encrypted_over_compressed_blob() {
        // Live path layout: compress, then encrypt. The file starts with the
        // encryption magic, not the compression one.
        let tmp = tempfile::tempdir().unwrap();
        let mut m = meta(4096);
        m.encryption = Some(enc("AES256"));
        m.compression = Some(BlobCompressionInfo {
            algorithm: CompressionAlgorithm::Zstd,
            chunk_size: 1024,
            original_size: 4096,
            compressed_size: 64,
        });
        let body = [b"AENC".as_slice(), &[7u8; 60]].concat();
        let (blob, mp) = fixture(tmp.path(), ID1, &m, Some(&body)).await;
        let sidecar_before = fs::read(&mp).await.unwrap();

        run_decompress_existing(&test_config(tmp.path()), false, None)
            .await
            .unwrap();

        assert_eq!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(fs::read(&mp).await.unwrap(), sidecar_before);
    }

    // ----- Crash-safe rewrite -----

    /// Reads a blob back through the real store stack (FsBlobStore under a
    /// CompressingBlobStore), exactly as the server would.
    async fn read_back(tmp: &Path, id: &str) -> Vec<u8> {
        use arca_core::store::{BlobStore, MetadataStore};
        use tokio_stream::StreamExt;
        let fs_store = Arc::new(FsBlobStore::new(&tmp.join("blobs"), 2).await.unwrap());
        let metadata: Arc<dyn MetadataStore> =
            Arc::new(arca_storage::SqliteStore::open(&tmp.join("read-back.db")).await.unwrap());
        let store = arca_storage::CompressingBlobStore::new(
            fs_store.clone() as Arc<dyn BlobStore>,
            fs_store,
            metadata,
        );
        let mut stream = store.get(&BlobId(id.to_string()), None).await.unwrap().stream;
        let mut out = Vec::new();
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    fn dir_names(path: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn compress_and_decompress_read_back_and_leave_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let (blob, mp) = fixture(tmp.path(), ID1, &meta(body.len() as u64), Some(&body)).await;
        let config = test_config(tmp.path());
        let expected = vec![ID1.to_string(), format!("{ID1}.meta")];

        run_compress_existing(&config, false, None, Some("zstd")).await.unwrap();
        assert!(read_sidecar(&mp).await.unwrap().compression.is_some());
        assert_ne!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(read_back(tmp.path(), ID1).await, body);
        assert_eq!(dir_names(&blob), expected);

        run_decompress_existing(&config, false, None).await.unwrap();
        assert_eq!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(read_back(tmp.path(), ID1).await, body);
        assert_eq!(dir_names(&blob), expected);
    }

    #[tokio::test]
    async fn failed_rewrite_keeps_original_blob_and_sidecar_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let (blob, mp) = fixture(tmp.path(), ID1, &meta(body.len() as u64), Some(&body)).await;
        let sidecar_before = fs::read(&mp).await.unwrap();
        // Make the journal write (the first step of the commit) fail.
        let pending = inplace::pending_sidecar_path(&mp);
        fs::create_dir(&pending).await.unwrap();
        fs::write(pending.join("keep"), b"x").await.unwrap();

        let err = run_compress_existing(&test_config(tmp.path()), false, None, Some("zstd")).await;

        assert!(err.is_err());
        assert_eq!(fs::read(&blob).await.unwrap(), body);
        assert_eq!(fs::read(&mp).await.unwrap(), sidecar_before);
        assert_eq!(read_back(tmp.path(), ID1).await, body);
        assert!(!inplace::rewrite_tmp_path(&blob).exists(), "temp blob must be removed");
    }

    /// Runs the compress steps up to (and excluding) the commit, like a process
    /// that dies right before it: returns the new sidecar.
    async fn stage_compress(blob: &Path, body_len: u64, old: &SidecarMeta) -> SidecarMeta {
        let info = compress_blob_to_tmp(
            blob,
            CompressionAlgorithm::Zstd,
            3,
            arca_storage::compressed_blob::DEFAULT_CHUNK_SIZE,
            body_len,
        )
        .await
        .unwrap();
        let mut new = old.clone();
        new.compression = Some(info);
        new
    }

    #[tokio::test]
    async fn crash_after_journal_before_blob_rename_is_discarded_on_rerun() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let m = meta(body.len() as u64);
        let (blob, mp) = fixture(tmp.path(), ID1, &m, Some(&body)).await;
        let new = stage_compress(&blob, body.len() as u64, &m).await;
        fs::write(inplace::pending_sidecar_path(&mp), serde_json::to_string(&new).unwrap())
            .await
            .unwrap();
        // Old pair intact, as after the crash.
        assert_eq!(read_back(tmp.path(), ID1).await, body);

        run_compress_existing(&test_config(tmp.path()), false, None, Some("zstd"))
            .await
            .unwrap();

        assert!(read_sidecar(&mp).await.unwrap().compression.is_some());
        assert_eq!(read_back(tmp.path(), ID1).await, body);
        assert_eq!(dir_names(&blob), vec![ID1.to_string(), format!("{ID1}.meta")]);
    }

    #[tokio::test]
    async fn crash_between_blob_and_sidecar_rename_is_completed_on_rerun() {
        // The dangerous window: compressed blob in place, sidecar still old.
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let m = meta(body.len() as u64);
        let (blob, mp) = fixture(tmp.path(), ID1, &m, Some(&body)).await;
        let new = stage_compress(&blob, body.len() as u64, &m).await;
        fs::write(inplace::pending_sidecar_path(&mp), serde_json::to_string(&new).unwrap())
            .await
            .unwrap();
        fs::rename(inplace::rewrite_tmp_path(&blob), &blob).await.unwrap();
        assert!(read_sidecar(&mp).await.unwrap().compression.is_none());

        let config = test_config(tmp.path());
        run_compress_existing(&config, false, None, Some("zstd")).await.unwrap();

        assert!(read_sidecar(&mp).await.unwrap().compression.is_some());
        assert_eq!(read_back(tmp.path(), ID1).await, body, "no double compression");
        assert_eq!(dir_names(&blob), vec![ID1.to_string(), format!("{ID1}.meta")]);
    }

    #[tokio::test]
    async fn dry_run_does_not_touch_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        let (blob, _mp) = fixture(tmp.path(), ID1, &meta(body.len() as u64), Some(&body)).await;
        fs::write(inplace::rewrite_tmp_path(&blob), b"partial").await.unwrap();

        run_compress_existing(&test_config(tmp.path()), true, None, Some("zstd"))
            .await
            .unwrap();

        assert!(inplace::rewrite_tmp_path(&blob).exists());
    }

    fn marker_present(tmp: &Path) -> bool {
        crate::rewrite_marker::marker_path(tmp).exists()
    }

    #[tokio::test]
    async fn marker_not_written_in_dry_run_and_removed_after_clean_run() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        fixture(tmp.path(), ID1, &meta(body.len() as u64), Some(&body)).await;
        let config = test_config(tmp.path());

        run_compress_existing(&config, true, None, Some("zstd")).await.unwrap();
        assert!(!marker_present(tmp.path()));

        // A marker seeded directly (independent of `begin`) must be removed by
        // `finish` after a clean run, and left alone by a dry run.
        let marker = crate::rewrite_marker::Marker {
            command: "compress-existing".into(),
            args: vec![],
            started_at: "2026-01-01T00:00:00Z".into(),
            pid: 1,
        };
        let seed = || {
            std::fs::write(
                crate::rewrite_marker::marker_path(tmp.path()),
                serde_json::to_vec(&marker).unwrap(),
            )
            .unwrap()
        };
        seed();
        run_compress_existing(&config, true, None, Some("zstd")).await.unwrap();
        assert!(marker_present(tmp.path()));

        run_compress_existing(&config, false, None, Some("zstd")).await.unwrap();
        assert!(!marker_present(tmp.path()));
    }

    #[tokio::test]
    async fn marker_kept_when_the_run_fails_midway_and_cleared_by_same_command() {
        let tmp = tempfile::tempdir().unwrap();
        let body = payload();
        fixture(tmp.path(), ID1, &meta(body.len() as u64), Some(&body)).await;
        let config = test_config(tmp.path());

        // Unknown algorithm: fails on the first candidate, after the marker.
        assert!(run_compress_existing(&config, false, Some("b"), Some("bogus")).await.is_err());
        let crate::rewrite_marker::MarkerState::Present(m) =
            crate::rewrite_marker::read(tmp.path()).unwrap()
        else {
            panic!("marker must stay");
        };
        assert_eq!(m.command, "compress-existing");
        assert_eq!(m.args, vec!["--bucket", "b", "--algorithm", "bogus"]);

        // Another rewrite command is refused, and does not touch the blob.
        let err = run_decompress_existing(&config, false, None).await.unwrap_err().to_string();
        assert!(err.contains("arca compress-existing"), "{err}");
        assert!(marker_present(tmp.path()));

        // Dry run is still allowed.
        run_decompress_existing(&config, true, None).await.unwrap();

        // The same command clears it.
        run_compress_existing(&config, false, None, Some("zstd")).await.unwrap();
        assert!(!marker_present(tmp.path()));
    }
}
