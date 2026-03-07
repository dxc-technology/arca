//! Multipart upload handlers.

use axum::body::Body;
use axum::response::Response;
use http::header;
use http::StatusCode;
use md5::{Digest, Md5};

use arca_core::s3::xml_types;
use arca_core::store::{ByteStream, SidecarMeta};
use arca_core::types::{BlobId, MultipartUploadRecord, ObjectRecord, PartRecord};
use arca_core::{S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::{internal_error_response, s3_error_response};

/// Minimum part size (5 MB) — all parts except the last must be at least this size.
const MIN_PART_SIZE: u64 = 5_242_880;

/// Maximum part number (S3 allows 1-10000).
const MAX_PART_NUMBER: u32 = 10_000;

/// POST /{bucket}/{key}?uploads — CreateMultipartUpload
pub async fn create_multipart_upload(
    state: AppState,
    bucket: String,
    key: String,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}/{key}");

    // Check bucket exists.
    match state.metadata.head_bucket(&bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    // TECHDEBT(TD-008): Content-Type captured from CreateMultipartUpload init request.
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let upload_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();

    let record = MultipartUploadRecord {
        upload_id: upload_id.clone(),
        bucket: bucket.clone(),
        key: key.clone(),
        content_type,
        initiated_at: now,
    };

    if let Err(e) = state.metadata.create_multipart_upload(&record).await {
        return internal_error_response(e, &resource);
    }

    let xml = xml_types::initiate_multipart_upload_result(&bucket, &key, &upload_id);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build create_multipart_upload response")
}

/// PUT /{bucket}/{key}?partNumber=N&uploadId=X — UploadPart
pub async fn upload_part(
    state: AppState,
    bucket: String,
    key: String,
    part_number: u32,
    upload_id: String,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}/{key}");

    // Validate part number range.
    if part_number < 1 || part_number > MAX_PART_NUMBER {
        return s3_error_response(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!("Part number must be between 1 and {MAX_PART_NUMBER}"),
            &resource,
        ));
    }

    // Verify upload exists and matches bucket/key.
    let upload = match state.metadata.get_multipart_upload(&upload_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchUpload, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    if upload.bucket != bucket || upload.key != key {
        return s3_error_response(S3Error::new(S3ErrorCode::NoSuchUpload, &resource));
    }

    let headers = request.headers().clone();
    let body = request.into_body();
    let stream = super::body::body_to_byte_stream(body, &headers);

    // Write part blob.
    let blob_id = BlobId::new();
    let put_result = match state.blob.put(&blob_id, stream).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Insert part record (returns old for cleanup).
    let part = PartRecord {
        upload_id: upload_id.clone(),
        part_number,
        blob_id,
        size: put_result.size,
        etag: put_result.etag.clone(),
    };
    let old_part = match state.metadata.put_part(&part).await {
        Ok(old) => old,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Clean up old part blob if re-uploading same part number.
    if let Some(old) = old_part {
        if let Err(e) = state.blob.delete(&old.blob_id).await {
            tracing::warn!(error = %e, "Failed to delete old part blob");
        }
    }

    let etag = format!("\"{}\"", put_result.etag);
    Response::builder()
        .status(StatusCode::OK)
        .header("ETag", &etag)
        .body(Body::empty())
        .expect("build upload_part response")
}

/// POST /{bucket}/{key}?uploadId=X — CompleteMultipartUpload
pub async fn complete_multipart_upload(
    state: AppState,
    bucket: String,
    key: String,
    upload_id: String,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}/{key}");

    // Verify upload exists.
    let upload = match state.metadata.get_multipart_upload(&upload_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchUpload, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    if upload.bucket != bucket || upload.key != key {
        return s3_error_response(S3Error::new(S3ErrorCode::NoSuchUpload, &resource));
    }

    // Read and parse XML body (1 MB limit).
    let body_bytes = match axum::body::to_bytes(request.into_body(), 1_048_576).await {
        Ok(b) => b,
        Err(_) => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Request body too large or invalid",
                &resource,
            ));
        }
    };
    let body_str = match std::str::from_utf8(&body_bytes) {
        Ok(s) => s,
        Err(_) => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Request body is not valid UTF-8",
                &resource,
            ));
        }
    };

    let complete_body = match xml_types::parse_complete_multipart_upload(body_str) {
        Ok(b) => b,
        Err(_) => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Invalid CompleteMultipartUpload XML",
                &resource,
            ));
        }
    };

    // Validate parts in ascending order and non-empty.
    if complete_body.parts.is_empty() {
        return s3_error_response(S3Error::new(S3ErrorCode::MalformedXML, &resource));
    }

    for i in 1..complete_body.parts.len() {
        if complete_body.parts[i].part_number <= complete_body.parts[i - 1].part_number {
            return s3_error_response(S3Error::new(S3ErrorCode::InvalidPartOrder, &resource));
        }
    }

    // Get stored parts.
    let stored_parts = match state.metadata.list_parts(&upload_id).await {
        Ok(p) => p,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Build lookup map of stored parts by part_number.
    let parts_map: std::collections::HashMap<u32, &PartRecord> =
        stored_parts.iter().map(|p| (p.part_number, p)).collect();

    // Match client ETags against stored parts and collect matched parts in order.
    let mut matched_parts: Vec<&PartRecord> = Vec::with_capacity(complete_body.parts.len());
    for client_part in &complete_body.parts {
        let stored = match parts_map.get(&client_part.part_number) {
            Some(p) => p,
            None => {
                return s3_error_response(S3Error::new(S3ErrorCode::InvalidPart, &resource));
            }
        };

        // Strip quotes from client ETag for comparison.
        let client_etag = client_part.etag.trim_matches('"');
        if client_etag != stored.etag {
            return s3_error_response(S3Error::new(S3ErrorCode::InvalidPart, &resource));
        }

        matched_parts.push(stored);
    }

    // Check part sizes: all parts except the last must be >= 5 MB.
    for (i, part) in matched_parts.iter().enumerate() {
        let is_last = i == matched_parts.len() - 1;
        if !is_last && part.size < MIN_PART_SIZE {
            return s3_error_response(S3Error::new(S3ErrorCode::EntityTooSmall, &resource));
        }
    }

    // Chain part streams into a final blob.
    let final_blob_id = BlobId::new();
    let mut combined_stream: ByteStream = Box::pin(tokio_stream::empty());
    for part in &matched_parts {
        let get_result = match state.blob.get(&part.blob_id, None).await {
            Ok(r) => r,
            Err(e) => return internal_error_response(e, &resource),
        };
        combined_stream = Box::pin(tokio_stream::StreamExt::chain(
            combined_stream,
            get_result.stream,
        ));
    }

    let put_result = match state.blob.put(&final_blob_id, combined_stream).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Compute composite ETag: hex(MD5(binary_MD5(part1) || binary_MD5(part2) || ...))-{count}
    let mut md5_concat = Vec::new();
    for part in &matched_parts {
        let part_md5_bytes = match hex::decode(&part.etag) {
            Ok(b) => b,
            Err(_) => {
                return s3_error_response(S3Error::with_message(
                    S3ErrorCode::InternalError,
                    "Invalid part ETag format",
                    &resource,
                ));
            }
        };
        md5_concat.extend_from_slice(&part_md5_bytes);
    }
    let composite_hash = Md5::digest(&md5_concat);
    let composite_etag = format!("{}-{}", hex::encode(composite_hash), matched_parts.len());

    let now = chrono::Utc::now();

    // Write sidecar for the final blob.
    let sidecar = SidecarMeta {
        bucket: bucket.clone(),
        key: key.clone(),
        size: put_result.size,
        etag: composite_etag.clone(),
        content_type: upload.content_type.clone(),
        last_modified: now.to_rfc3339(),
    };
    if let Err(e) = state.blob.write_sidecar(&final_blob_id, &sidecar).await {
        return internal_error_response(e, &resource);
    }

    // Insert object record (returns old for cleanup).
    let record = ObjectRecord {
        bucket: bucket.clone(),
        key: key.clone(),
        blob_id: final_blob_id,
        size: put_result.size,
        etag: composite_etag.clone(),
        content_type: upload.content_type,
        last_modified: now,
    };
    let old = match state.metadata.put_object(&record).await {
        Ok(old) => old,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Clean up old blob if overwriting.
    if let Some(old_record) = old {
        if let Err(e) = state.blob.delete(&old_record.blob_id).await {
            tracing::warn!(error = %e, "Failed to delete old blob during multipart complete");
        }
    }

    // Delete upload + parts from DB and clean up part blobs.
    let old_parts = match state.metadata.delete_multipart_upload(&upload_id).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "Failed to delete multipart upload record");
            Vec::new()
        }
    };
    for part in &old_parts {
        if let Err(e) = state.blob.delete(&part.blob_id).await {
            tracing::warn!(error = %e, part_number = part.part_number, "Failed to delete part blob");
        }
    }

    let xml = xml_types::complete_multipart_upload_result(&bucket, &key, &composite_etag);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build complete_multipart_upload response")
}

/// DELETE /{bucket}/{key}?uploadId=X — AbortMultipartUpload
pub async fn abort_multipart_upload(
    state: AppState,
    bucket: String,
    key: String,
    upload_id: String,
) -> Response {
    let resource = format!("/{bucket}/{key}");

    // Verify upload exists.
    match state.metadata.get_multipart_upload(&upload_id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchUpload, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    // Delete upload + parts from DB.
    let parts = match state.metadata.delete_multipart_upload(&upload_id).await {
        Ok(p) => p,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Clean up part blobs (log errors, don't fail).
    for part in &parts {
        if let Err(e) = state.blob.delete(&part.blob_id).await {
            tracing::warn!(error = %e, part_number = part.part_number, "Failed to delete part blob during abort");
        }
    }

    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("build abort_multipart_upload response")
}

