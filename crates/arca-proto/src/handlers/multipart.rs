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

use super::object::extract_metadata;

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

    let metadata = extract_metadata(request.headers());

    let upload_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now();

    let record = MultipartUploadRecord {
        upload_id: upload_id.clone(),
        bucket: bucket.clone(),
        key: key.clone(),
        content_type,
        initiated_at: now,
        metadata,
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

    // Write sidecar for the part blob so that EncryptingBlobStore.get()
    // can detect and decrypt it during CompleteMultipartUpload assembly.
    if put_result.encryption.is_some() {
        let sidecar = SidecarMeta {
            bucket: bucket.clone(),
            key: format!("{key}#{upload_id}#{part_number}"),
            size: put_result.size,
            etag: put_result.etag.clone(),
            content_type: None,
            last_modified: chrono::Utc::now().to_rfc3339(),
            metadata: std::collections::HashMap::new(),
            encryption: put_result.encryption.clone(),
        };
        if let Err(e) = state.blob.write_sidecar(&blob_id, &sidecar).await {
            tracing::warn!(error = %e, "Failed to write part sidecar");
        }
    }

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

    // Verify upload exists. If already completed, return idempotent success
    // by looking up the assembled object (S3 CompleteMultipartUpload is idempotent).
    let upload = match state.metadata.get_multipart_upload(&upload_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            // Upload already completed? Return success if object exists.
            if let Ok(Some(obj)) = state.metadata.get_object(&bucket, &key).await {
                if obj.etag.contains('-') {
                    let xml = xml_types::complete_multipart_upload_result(
                        &bucket, &key, &obj.etag,
                    );
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header("Content-Type", "application/xml")
                        .body(Body::from(xml))
                        .expect("build idempotent complete_multipart_upload response");
                }
            }
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchUpload, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    if upload.bucket != bucket || upload.key != key {
        return s3_error_response(S3Error::new(S3ErrorCode::NoSuchUpload, &resource));
    }

    // Check conditional headers (If-Match, If-None-Match) against existing object.
    let if_match = request
        .headers()
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let if_none_match = request
        .headers()
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    if let Some(resp) =
        check_complete_conditionals(&state, &bucket, &key, &if_match, &if_none_match, &resource)
            .await
    {
        return resp;
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

    let mut complete_body = match xml_types::parse_complete_multipart_upload(body_str) {
        Ok(b) => b,
        Err(_) => {
            return s3_error_response(S3Error::new(S3ErrorCode::MalformedXML, &resource));
        }
    };

    // Validate non-empty, sort by part number, and deduplicate (keep last entry per part).
    if complete_body.parts.is_empty() {
        return s3_error_response(S3Error::new(S3ErrorCode::MalformedXML, &resource));
    }

    // Stable sort + reverse dedup: keep the last-submitted entry for each part number.
    complete_body.parts.sort_by_key(|p| p.part_number);
    complete_body.parts.reverse();
    complete_body.parts.dedup_by_key(|p| p.part_number);
    complete_body.parts.reverse();

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
        metadata: upload.metadata.clone(),
        encryption: put_result.encryption.clone(),
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
        metadata: upload.metadata,
        encryption_algorithm: put_result.encryption.as_ref().map(|e| e.algorithm.clone()),
        encryption_key_id: put_result.encryption.as_ref().map(|e| e.key_id.clone()),
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

/// Checks If-Match / If-None-Match conditionals on CompleteMultipartUpload.
async fn check_complete_conditionals(
    state: &AppState,
    bucket: &str,
    key: &str,
    if_match: &Option<String>,
    if_none_match: &Option<String>,
    resource: &str,
) -> Option<Response> {
    if if_match.is_none() && if_none_match.is_none() {
        return None;
    }

    let existing = match state.metadata.get_object(bucket, key).await {
        Ok(obj) => obj,
        Err(_) => return None,
    };

    // If-Match: object must exist and ETag must match.
    if let Some(expected) = if_match {
        match &existing {
            Some(obj) => {
                let quoted_etag = format!("\"{}\"", obj.etag);
                if !super::object::etag_matches(expected, &quoted_etag) {
                    return Some(s3_error_response(S3Error::new(
                        S3ErrorCode::PreconditionFailed,
                        resource,
                    )));
                }
            }
            None => {
                return Some(s3_error_response(S3Error::new(
                    S3ErrorCode::NoSuchKey,
                    resource,
                )));
            }
        }
    }

    // If-None-Match: object must not exist or ETag must not match.
    if let Some(expected) = if_none_match {
        if let Some(obj) = &existing {
            let quoted_etag = format!("\"{}\"", obj.etag);
            if super::object::etag_matches(expected, &quoted_etag) {
                return Some(s3_error_response(S3Error::new(
                    S3ErrorCode::PreconditionFailed,
                    resource,
                )));
            }
        }
    }

    None
}

