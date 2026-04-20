//! Offline `arca compress-existing` / `arca decompress-existing`.
//!
//! Walks the blobs directory, reads each `.meta` sidecar, and either
//! compresses or decompresses the associated blob file in place. The
//! sidecar is updated atomically after the blob is swapped.

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
use arca_storage::compression::auto::pick_auto;
use arca_storage::compression::format::{self, FOOTER_COUNT_SIZE, HEADER_SIZE};
use arca_storage::compression::stream::{CompressingStream, DecompressingStream};
use arca_storage::FsBlobStore;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

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

    let mut compressed = 0u64;
    let mut skipped = 0u64;
    let mut ratio_plain: u64 = 0;
    let mut ratio_comp: u64 = 0;

    for meta_path in meta_files {
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

        let info = compress_blob_in_place(
            &blob_path,
            algorithm,
            level,
            arca_storage::compressed_blob::DEFAULT_CHUNK_SIZE,
            meta.size,
        )
        .await
        .with_context(|| format!("compressing {}", blob_path.display()))?;

        meta.compression = Some(info.clone());
        write_sidecar(&meta_path, &meta).await?;

        ratio_plain += info.original_size;
        ratio_comp += info.compressed_size;
        compressed += 1;
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

    let mut decompressed = 0u64;
    let mut skipped = 0u64;

    for meta_path in meta_files {
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

        decompress_blob_in_place(&blob_path, &info)
            .await
            .with_context(|| format!("decompressing {}", blob_path.display()))?;

        meta.compression = None;
        write_sidecar(&meta_path, &meta).await?;
        decompressed += 1;
    }

    println!(
        "\nDone. decompressed={} skipped={}",
        decompressed, skipped
    );
    Ok(())
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

/// Read plaintext → write compressed to a `.compressing.tmp` → rename.
async fn compress_blob_in_place(
    blob_path: &Path,
    algorithm: CompressionAlgorithm,
    level: i32,
    chunk_size: u32,
    plaintext_size: u64,
) -> Result<BlobCompressionInfo> {
    let tmp = blob_path.with_extension("compressing.tmp");
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
    drop(out);

    let compressed_size = fs::metadata(&tmp).await?.len();
    fs::rename(&tmp, blob_path).await?;
    Ok(BlobCompressionInfo {
        algorithm,
        chunk_size,
        original_size: plaintext_size,
        compressed_size,
    })
}

/// Read compressed → write plaintext to a `.decompressing.tmp` → rename.
async fn decompress_blob_in_place(
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

    let tmp = blob_path.with_extension("decompressing.tmp");
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
    drop(out);

    let new_size = fs::metadata(&tmp).await?.len();
    if new_size != info.original_size {
        anyhow::bail!(
            "decompressed size {} != sidecar original_size {}",
            new_size,
            info.original_size
        );
    }
    fs::rename(&tmp, blob_path).await?;
    Ok(())
}
