//! Sidecar helpers shared by the handlers that write blobs.
//!
//! A freshly written blob is durable only once its sidecar has been written:
//! `FsBlobStore` does not fsync the directory after publishing the blob file,
//! the sidecar write (same directory) does it for both entries. A handler must
//! therefore write the sidecar, and see it succeed, before it commits the
//! metadata row that makes the blob reachable.

use axum::response::Response;

use arca_core::store::{BlobPutResult, BlobStore, SidecarMeta};
use arca_core::types::BlobId;

use crate::xml::error_response::internal_error_response;

/// Sidecar of a multipart part blob, shared by UploadPart and UploadPartCopy.
///
/// Every part gets one, encrypted or not: `EncryptingBlobStore::get` reads it
/// to find the part's DEK, and `FsBlobStore::concat` needs one on every part
/// (etag + size) to assemble a composite instead of copying the bytes.
pub(crate) fn part_sidecar(
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: u32,
    put_result: &BlobPutResult,
) -> SidecarMeta {
    SidecarMeta {
        bucket: bucket.to_string(),
        key: format!("{key}#{upload_id}#{part_number}"),
        size: put_result.size,
        etag: put_result.etag.clone(),
        content_type: None,
        last_modified: chrono::Utc::now().to_rfc3339(),
        metadata: std::collections::HashMap::new(),
        encryption: put_result.encryption.clone(),
        compression: None,
        version_id: None,
        composite: None,
    }
}

/// Writes the sidecar of a just-written blob, which also makes the blob
/// durable (see the module docs).
///
/// On failure the blob is discarded (its own file and sidecar only, never the
/// parts a composite points at) and the error response is returned, so the
/// caller answers with it instead of committing a row for a blob that may not
/// survive a power loss.
pub(crate) async fn write_sidecar_or_discard(
    blob: &dyn BlobStore,
    blob_id: &BlobId,
    meta: &SidecarMeta,
    resource: &str,
) -> Result<(), Response> {
    match blob.write_sidecar(blob_id, meta).await {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Err(del_err) = blob.delete_assembled(blob_id).await {
                tracing::warn!(error = %del_err, "Failed to delete blob after failed sidecar write");
            }
            Err(internal_error_response(e, resource))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use arca_core::error::ArcaError;
    use arca_core::store::{BlobEncryptionInfo, BlobGetResult, ByteRange, ByteStream};

    /// Blob store double: records deletes and sidecar writes, and can be told
    /// to fail every sidecar write.
    #[derive(Default)]
    struct RecordingBlobStore {
        fail_sidecar: bool,
        sidecars: Mutex<Vec<String>>,
        deleted: Mutex<Vec<String>>,
        deleted_assembled: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl BlobStore for RecordingBlobStore {
        async fn put(&self, _: &BlobId, _: ByteStream) -> Result<BlobPutResult, ArcaError> {
            unimplemented!("not used by these tests")
        }

        async fn get(&self, _: &BlobId, _: Option<ByteRange>) -> Result<BlobGetResult, ArcaError> {
            Err(ArcaError::Internal("not used by these tests".into()))
        }

        async fn delete(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
            self.deleted.lock().unwrap().push(blob_id.0.clone());
            Ok(())
        }

        async fn delete_assembled(&self, blob_id: &BlobId) -> Result<(), ArcaError> {
            self.deleted_assembled.lock().unwrap().push(blob_id.0.clone());
            Ok(())
        }

        async fn write_sidecar(&self, blob_id: &BlobId, _: &SidecarMeta) -> Result<(), ArcaError> {
            if self.fail_sidecar {
                return Err(ArcaError::Internal("disk full".into()));
            }
            self.sidecars.lock().unwrap().push(blob_id.0.clone());
            Ok(())
        }
    }

    fn put_result(encryption: Option<BlobEncryptionInfo>) -> BlobPutResult {
        BlobPutResult {
            size: 42,
            etag: "0123456789abcdef0123456789abcdef".into(),
            encryption,
            compression: None,
            composite_parts: None,
        }
    }

    #[test]
    fn part_sidecar_carries_part_identity_etag_and_size() {
        let meta = part_sidecar("bkt", "dir/obj", "up-1", 7, &put_result(None));
        assert_eq!(meta.bucket, "bkt");
        assert_eq!(meta.key, "dir/obj#up-1#7");
        assert_eq!(meta.size, 42);
        assert_eq!(meta.etag, "0123456789abcdef0123456789abcdef");
        // A plain part: nothing that would push FsBlobStore::concat off its
        // composite fast path.
        assert!(meta.encryption.is_none());
        assert!(meta.compression.is_none());
        assert!(meta.composite.is_none());
        assert!(meta.version_id.is_none());
        assert!(meta.content_type.is_none());
        assert!(meta.metadata.is_empty());
    }

    #[test]
    fn part_sidecar_keeps_encryption_info() {
        let enc = BlobEncryptionInfo {
            algorithm: "AES256".into(),
            encrypted_dek: "dek".into(),
            dek_nonce: "nonce".into(),
            nonce_prefix: "pfx".into(),
            key_id: "kid".into(),
        };
        let meta = part_sidecar("b", "k", "u", 1, &put_result(Some(enc)));
        let got = meta.encryption.expect("encryption info must be kept");
        assert_eq!(got.algorithm, "AES256");
        assert_eq!(got.encrypted_dek, "dek");
        assert_eq!(got.key_id, "kid");
    }

    #[tokio::test]
    async fn write_sidecar_or_discard_success_keeps_the_blob() {
        let store = RecordingBlobStore::default();
        let id = BlobId("blob-1".into());
        let meta = part_sidecar("b", "k", "u", 1, &put_result(None));

        let res = write_sidecar_or_discard(&store, &id, &meta, "/b/k").await;

        assert!(res.is_ok());
        assert_eq!(*store.sidecars.lock().unwrap(), vec!["blob-1".to_string()]);
        assert!(store.deleted.lock().unwrap().is_empty());
        assert!(store.deleted_assembled.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_sidecar_or_discard_failure_deletes_blob_and_returns_500() {
        let store = RecordingBlobStore {
            fail_sidecar: true,
            ..Default::default()
        };
        let id = BlobId("blob-2".into());
        let meta = part_sidecar("b", "k", "u", 1, &put_result(None));

        let resp = write_sidecar_or_discard(&store, &id, &meta, "/b/k")
            .await
            .expect_err("a failed sidecar write must fail the request");

        assert_eq!(resp.status(), http::StatusCode::INTERNAL_SERVER_ERROR);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("<Code>InternalError</Code>"), "body: {body}");
        // The blob goes away without cascading into composite parts.
        assert_eq!(*store.deleted_assembled.lock().unwrap(), vec!["blob-2".to_string()]);
        assert!(store.deleted.lock().unwrap().is_empty());
    }
}
