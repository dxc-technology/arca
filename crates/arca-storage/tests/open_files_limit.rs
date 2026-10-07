//! Reading a composite blob with more parts than the process may open files.
//!
//! A 200 GB multipart object of 5995 parts could not be downloaded from a
//! server whose soft `RLIMIT_NOFILE` was 1024: every GET opened all its part
//! files before streaming the first byte and failed with EMFILE (os error 24).
//! This test lowers the limit to `LIMIT` and reads composites of `PARTS`
//! parts, plain and encrypted, plus the encrypted `concat` fallback, which
//! read every part the same way.
//!
//! It lowers a process-wide limit, so it lives in its own test binary and must
//! stay the only test in this file.

#![cfg(unix)]

use std::collections::HashMap;
use std::sync::Arc;

use arca_core::store::{BlobStore, ByteRange, ByteStream, SidecarMeta};
use arca_core::types::BlobId;
use arca_storage::encryption::keys::MasterKey;
use arca_storage::{EncryptingBlobStore, FsBlobStore};
use bytes::Bytes;
use tokio_stream::StreamExt;

const LIMIT: u64 = 256;
const PARTS: usize = 1000;
const PART_SIZE: usize = 64;

fn lower_open_files_limit(soft: u64) {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    lim.rlim_cur = soft.min(lim.rlim_max) as libc::rlim_t;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
}

fn sidecar(key: &str, size: u64, etag: String) -> SidecarMeta {
    SidecarMeta {
        bucket: "test".into(),
        key: key.into(),
        size,
        etag,
        content_type: None,
        last_modified: "2026-01-01T00:00:00Z".into(),
        metadata: HashMap::new(),
        encryption: None,
        compression: None,
        version_id: None,
        composite: None,
    }
}

/// Writes `PARTS` parts through `store` (plain or encrypted, as the store
/// does) and returns their ids and their concatenation.
async fn write_parts(store: &dyn BlobStore) -> (Vec<BlobId>, Vec<u8>) {
    let mut ids = Vec::with_capacity(PARTS);
    let mut full = Vec::with_capacity(PARTS * PART_SIZE);
    for i in 0..PARTS {
        let id = BlobId::new();
        let data: Vec<u8> = (0..PART_SIZE).map(|b| ((i * 31 + b) % 256) as u8).collect();
        let body: ByteStream = Box::pin(tokio_stream::iter(vec![Ok(Bytes::from(data.clone()))]));
        let r = store.put(&id, body).await.unwrap();
        let mut meta = sidecar(&format!("part-{i}"), r.size, r.etag);
        meta.encryption = r.encryption;
        store.write_sidecar(&id, &meta).await.unwrap();
        ids.push(id);
        full.extend_from_slice(&data);
    }
    (ids, full)
}

/// Assembles `part_ids` with `concat` and writes the resulting sidecar.
async fn assemble(store: &dyn BlobStore, part_ids: &[BlobId]) -> BlobId {
    let id = BlobId::new();
    let r = store.concat(part_ids, &id).await.unwrap();
    let mut meta = sidecar("assembled", r.size, r.etag);
    meta.encryption = r.encryption;
    meta.composite = r.composite_parts;
    store.write_sidecar(&id, &meta).await.unwrap();
    id
}

async fn read_all(store: &dyn BlobStore, id: &BlobId, range: Option<ByteRange>) -> Vec<u8> {
    let g = store.get(id, range).await.unwrap();
    let mut stream = std::pin::pin!(g.stream);
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(out.len() as u64, g.content_length);
    out
}

#[tokio::test]
async fn composite_reads_need_no_descriptor_per_part() {
    lower_open_files_limit(LIMIT);
    let dir = tempfile::tempdir().unwrap();
    let fs_store = FsBlobStore::new(dir.path().join("blobs"), 2).await.unwrap().with_fsync(false);
    let encrypted = EncryptingBlobStore::new(
        fs_store.clone(),
        Arc::new(MasterKey::from_bytes(&[0x42u8; 32]).unwrap()),
    );
    let range = ByteRange { start: 10, end: Some((PARTS * PART_SIZE - 10) as u64) };

    // Plain composite.
    let (ids, full) = write_parts(&fs_store).await;
    let plain = assemble(&fs_store, &ids).await;
    assert_eq!(read_all(&fs_store, &plain, None).await, full, "plain composite, full GET");
    assert_eq!(
        read_all(&fs_store, &plain, Some(range)).await,
        &full[10..=PARTS * PART_SIZE - 10],
        "plain composite, ranged GET"
    );

    // Encrypted composite.
    let (enc_ids, enc_full) = write_parts(&encrypted).await;
    let composite = assemble(&encrypted, &enc_ids).await;
    assert_eq!(read_all(&encrypted, &composite, None).await, enc_full, "encrypted composite, full GET");
    assert_eq!(
        read_all(&encrypted, &composite, Some(range)).await,
        &enc_full[10..=PARTS * PART_SIZE - 10],
        "encrypted composite, ranged GET"
    );

    // Encrypted concat fallback: a plain part among encrypted ones makes
    // `concat` decrypt every part and re-encrypt the stream into one blob.
    let mut mixed = enc_ids.clone();
    mixed[0] = ids[0].clone();
    let mut mixed_full = enc_full.clone();
    mixed_full[..PART_SIZE].copy_from_slice(&full[..PART_SIZE]);
    let assembled = assemble(&encrypted, &mixed).await;
    assert_eq!(read_all(&encrypted, &assembled, None).await, mixed_full, "encrypted concat fallback");
}
