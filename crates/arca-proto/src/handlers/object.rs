//! Object operation handlers.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use http::header;
use http::StatusCode;

use arca_core::s3::xml_types;
use arca_core::store::{ByteRange, SidecarMeta};
use arca_core::types::{BlobId, ObjectRecord};
use arca_core::{S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::{internal_error_response, s3_error_response};

/// Checks conditional headers (If-Match, If-None-Match, If-Modified-Since,
/// If-Unmodified-Since) against an object's ETag and Last-Modified.
///
/// Returns `Some(response)` if the condition fails (304 or 412), `None` if ok.
fn check_conditionals(
    headers: &http::HeaderMap,
    etag: &str,
    last_modified: &chrono::DateTime<chrono::Utc>,
    is_get_or_head: bool,
) -> Option<Response> {
    let quoted_etag = format!("\"{}\"", etag);

    // If-Match: succeed only if ETag matches one of the listed values.
    if let Some(val) = headers.get("if-match").and_then(|v| v.to_str().ok()) {
        if !etag_matches(val, &quoted_etag) {
            return Some(
                Response::builder()
                    .status(StatusCode::PRECONDITION_FAILED)
                    .body(Body::empty())
                    .expect("build 412 response"),
            );
        }
    }

    // If-None-Match: succeed only if ETag does NOT match any of the listed values.
    if let Some(val) = headers.get("if-none-match").and_then(|v| v.to_str().ok()) {
        if etag_matches(val, &quoted_etag) {
            if is_get_or_head {
                return Some(
                    Response::builder()
                        .status(StatusCode::NOT_MODIFIED)
                        .header("ETag", &quoted_etag)
                        .body(Body::empty())
                        .expect("build 304 response"),
                );
            } else {
                return Some(
                    Response::builder()
                        .status(StatusCode::PRECONDITION_FAILED)
                        .body(Body::empty())
                        .expect("build 412 response"),
                );
            }
        }
    }

    // If-Modified-Since: for GET/HEAD only — return 304 if not modified.
    if is_get_or_head {
        if let Some(val) = headers
            .get("if-modified-since")
            .and_then(|v| v.to_str().ok())
        {
            if let Ok(since) = httpdate::parse_http_date(val) {
                let since_dt: chrono::DateTime<chrono::Utc> = since.into();
                if *last_modified <= since_dt {
                    return Some(
                        Response::builder()
                            .status(StatusCode::NOT_MODIFIED)
                            .body(Body::empty())
                            .expect("build 304 response"),
                    );
                }
            }
        }
    }

    // If-Unmodified-Since: return 412 if modified after the given date.
    if let Some(val) = headers
        .get("if-unmodified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(since) = httpdate::parse_http_date(val) {
            let since_dt: chrono::DateTime<chrono::Utc> = since.into();
            if *last_modified > since_dt {
                return Some(
                    Response::builder()
                        .status(StatusCode::PRECONDITION_FAILED)
                        .body(Body::empty())
                        .expect("build 412 response"),
                );
            }
        }
    }

    None
}

/// Checks copy-source conditional headers for CopyObject / UploadPartCopy.
///
/// S3 uses `x-amz-copy-source-if-match`, `x-amz-copy-source-if-none-match`,
/// `x-amz-copy-source-if-modified-since`, `x-amz-copy-source-if-unmodified-since`
/// instead of the standard `If-Match` etc. All failures return 412.
fn check_copy_source_conditionals(
    headers: &http::HeaderMap,
    etag: &str,
    last_modified: &chrono::DateTime<chrono::Utc>,
) -> Option<Response> {
    let quoted_etag = format!("\"{}\"", etag);

    if let Some(val) = headers
        .get("x-amz-copy-source-if-match")
        .and_then(|v| v.to_str().ok())
    {
        if !etag_matches(val, &quoted_etag) {
            return Some(
                Response::builder()
                    .status(StatusCode::PRECONDITION_FAILED)
                    .body(Body::empty())
                    .expect("build 412 response"),
            );
        }
    }

    if let Some(val) = headers
        .get("x-amz-copy-source-if-none-match")
        .and_then(|v| v.to_str().ok())
    {
        if etag_matches(val, &quoted_etag) {
            return Some(
                Response::builder()
                    .status(StatusCode::PRECONDITION_FAILED)
                    .body(Body::empty())
                    .expect("build 412 response"),
            );
        }
    }

    if let Some(val) = headers
        .get("x-amz-copy-source-if-modified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(since) = httpdate::parse_http_date(val) {
            let since_dt: chrono::DateTime<chrono::Utc> = since.into();
            if *last_modified <= since_dt {
                return Some(
                    Response::builder()
                        .status(StatusCode::PRECONDITION_FAILED)
                        .body(Body::empty())
                        .expect("build 412 response"),
                );
            }
        }
    }

    if let Some(val) = headers
        .get("x-amz-copy-source-if-unmodified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let Ok(since) = httpdate::parse_http_date(val) {
            let since_dt: chrono::DateTime<chrono::Utc> = since.into();
            if *last_modified > since_dt {
                return Some(
                    Response::builder()
                        .status(StatusCode::PRECONDITION_FAILED)
                        .body(Body::empty())
                        .expect("build 412 response"),
                );
            }
        }
    }

    None
}

/// Checks if an ETag value matches the If-Match / If-None-Match header value.
/// The header can be `*` (matches everything) or a comma-separated list of ETags.
fn etag_matches(header_val: &str, etag: &str) -> bool {
    let trimmed = header_val.trim();
    if trimmed == "*" {
        return true;
    }
    trimmed
        .split(',')
        .any(|v| v.trim().trim_matches('"') == etag.trim_matches('"'))
}

/// PUT /{bucket}/{*key} — PutObject, CopyObject, UploadPart, or UploadPartCopy
///
/// Dispatches based on headers and query parameters:
/// - `?partNumber=N&uploadId=X` → UploadPart (or UploadPartCopy if x-amz-copy-source present)
/// - `x-amz-copy-source` header → CopyObject
/// - Otherwise → PutObject
pub async fn put_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    // Check for UploadPart / UploadPartCopy FIRST (query params take priority).
    if let Some(query) = request.uri().query() {
        let params: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let part_number = params.iter().find(|(k, _)| k == "partNumber");
        let upload_id = params.iter().find(|(k, _)| k == "uploadId");

        if let (Some((_, pn)), Some((_, uid))) = (part_number, upload_id) {
            let pn: u32 = match pn.parse() {
                Ok(n) => n,
                Err(_) => {
                    let resource = format!("/{bucket}/{key}");
                    return s3_error_response(S3Error::with_message(
                        S3ErrorCode::InvalidArgument,
                        "Invalid partNumber",
                        &resource,
                    ));
                }
            };

            // UploadPartCopy: partNumber + uploadId + x-amz-copy-source.
            if request.headers().contains_key("x-amz-copy-source") {
                return upload_part_copy(state, bucket, key, pn, uid.clone(), request).await;
            }

            return super::multipart::upload_part(state, bucket, key, pn, uid.clone(), request)
                .await;
        }
    }

    // Check for CopyObject (same PUT endpoint, distinguished by header).
    if request.headers().contains_key("x-amz-copy-source") {
        return copy_object(state, bucket, key, request).await;
    }

    let resource = format!("/{bucket}/{key}");

    // Check bucket exists.
    match state.metadata.head_bucket(&bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    // Check conditional headers against existing object (for optimistic concurrency).
    let has_conditionals = request.headers().contains_key("if-match")
        || request.headers().contains_key("if-none-match");
    if has_conditionals {
        let existing = match state.metadata.get_object(&bucket, &key).await {
            Ok(obj) => obj,
            Err(e) => return internal_error_response(e, &resource),
        };
        match existing {
            Some(ref obj) => {
                if let Some(resp) = check_conditionals(
                    request.headers(),
                    &obj.etag,
                    &obj.last_modified,
                    false,
                ) {
                    return resp;
                }
            }
            None => {
                // If-Match on non-existent object → 412.
                if request.headers().contains_key("if-match") {
                    return Response::builder()
                        .status(StatusCode::PRECONDITION_FAILED)
                        .body(Body::empty())
                        .expect("build 412 response");
                }
                // If-None-Match: * on non-existent object → proceed (condition met).
            }
        }
    }

    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let headers = request.headers().clone();
    let body = request.into_body();
    let stream = super::body::body_to_byte_stream(body, &headers);

    // Write blob.
    let blob_id = BlobId::new();
    let put_result = match state.blob.put(&blob_id, stream).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    let now = chrono::Utc::now();

    // Write sidecar (for disaster recovery).
    let sidecar = SidecarMeta {
        bucket: bucket.clone(),
        key: key.clone(),
        size: put_result.size,
        etag: put_result.etag.clone(),
        content_type: content_type.clone(),
        last_modified: now.to_rfc3339(),
    };
    if let Err(e) = state.blob.write_sidecar(&blob_id, &sidecar).await {
        return internal_error_response(e, &resource);
    }

    // Insert into metadata (returns old record for cleanup).
    let record = ObjectRecord {
        bucket,
        key,
        blob_id,
        size: put_result.size,
        etag: put_result.etag.clone(),
        content_type,
        last_modified: now,
    };
    let old = match state.metadata.put_object(&record).await {
        Ok(old) => old,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Clean up old blob if overwriting.
    if let Some(old_record) = old {
        if let Err(e) = state.blob.delete(&old_record.blob_id).await {
            tracing::warn!(error = %e, "Failed to delete old blob during overwrite");
        }
    }

    let etag = format!("\"{}\"", put_result.etag);
    Response::builder()
        .status(StatusCode::OK)
        .header("ETag", &etag)
        .body(Body::empty())
        .expect("build put_object response")
}

/// CopyObject — copies a source object to the destination bucket/key.
///
/// Streams the source blob through `BlobStore::get()` → `BlobStore::put()` to reuse
/// existing code paths and correctly compute the new blob's MD5.
async fn copy_object(
    state: AppState,
    dest_bucket: String,
    dest_key: String,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{dest_bucket}/{dest_key}");

    // Parse x-amz-copy-source header.
    let copy_source = request
        .headers()
        .get("x-amz-copy-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let (src_bucket, src_key) = match parse_copy_source(copy_source) {
        Some(parsed) => parsed,
        None => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Invalid x-amz-copy-source header",
                &resource,
            ));
        }
    };

    // Check destination bucket exists.
    match state.metadata.head_bucket(&dest_bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    // Check source bucket exists.
    match state.metadata.head_bucket(&src_bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(
                S3ErrorCode::NoSuchBucket,
                format!("/{src_bucket}/{src_key}"),
            ));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    // Get source object record.
    let src_record = match state.metadata.get_object(&src_bucket, &src_key).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return s3_error_response(S3Error::new(
                S3ErrorCode::NoSuchKey,
                format!("/{src_bucket}/{src_key}"),
            ));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    // Check copy-source conditional headers against source object.
    // CopyObject uses x-amz-copy-source-if-match etc. instead of If-Match.
    if let Some(resp) = check_copy_source_conditionals(
        request.headers(),
        &src_record.etag,
        &src_record.last_modified,
    ) {
        return resp;
    }

    // Parse x-amz-metadata-directive (default: COPY).
    let metadata_directive = request
        .headers()
        .get("x-amz-metadata-directive")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("COPY");

    // S3 requires REPLACE directive when copying an object to itself.
    if src_bucket == dest_bucket
        && src_key == dest_key
        && !metadata_directive.eq_ignore_ascii_case("REPLACE")
    {
        return s3_error_response(S3Error::with_message(
            S3ErrorCode::InvalidRequest,
            "This copy request is illegal because it is trying to copy an object to itself without changing the object's metadata, storage class, website redirect location or encryption attributes.",
            &resource,
        ));
    }

    // Stream source blob through get → put to create a new copy.
    let get_result = match state.blob.get(&src_record.blob_id, None).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    let new_blob_id = BlobId::new();
    let put_result = match state.blob.put(&new_blob_id, get_result.stream).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    let now = chrono::Utc::now();

    // Preserve source content-type.
    let content_type = src_record.content_type.clone();

    // Write sidecar.
    let sidecar = SidecarMeta {
        bucket: dest_bucket.clone(),
        key: dest_key.clone(),
        size: put_result.size,
        etag: put_result.etag.clone(),
        content_type: content_type.clone(),
        last_modified: now.to_rfc3339(),
    };
    if let Err(e) = state.blob.write_sidecar(&new_blob_id, &sidecar).await {
        return internal_error_response(e, &resource);
    }

    // Insert metadata record.
    let record = ObjectRecord {
        bucket: dest_bucket,
        key: dest_key,
        blob_id: new_blob_id,
        size: put_result.size,
        etag: put_result.etag.clone(),
        content_type,
        last_modified: now,
    };
    let old = match state.metadata.put_object(&record).await {
        Ok(old) => old,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Clean up old blob if overwriting.
    if let Some(old_record) = old {
        if let Err(e) = state.blob.delete(&old_record.blob_id).await {
            tracing::warn!(error = %e, "Failed to delete old blob during copy overwrite");
        }
    }

    // CopyObject returns XML body (not just headers like PutObject).
    let xml = xml_types::copy_object_result(&put_result.etag, &now);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build copy_object response")
}

/// PUT /{bucket}/{key}?partNumber=N&uploadId=X with x-amz-copy-source — UploadPartCopy
///
/// Copies data from a source object into a multipart upload part.
async fn upload_part_copy(
    state: AppState,
    bucket: String,
    key: String,
    part_number: u32,
    upload_id: String,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}/{key}");

    // Parse x-amz-copy-source header.
    let copy_source = request
        .headers()
        .get("x-amz-copy-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let (src_bucket, src_key) = match parse_copy_source(copy_source) {
        Some(parsed) => parsed,
        None => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Invalid x-amz-copy-source header",
                &resource,
            ));
        }
    };

    // Parse optional x-amz-copy-source-range header.
    let copy_range = request
        .headers()
        .get("x-amz-copy-source-range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Verify the multipart upload exists and matches bucket/key.
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

    // Get source object.
    let src_record = match state.metadata.get_object(&src_bucket, &src_key).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return s3_error_response(S3Error::new(
                S3ErrorCode::NoSuchKey,
                format!("/{src_bucket}/{src_key}"),
            ));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    // Parse the copy-source-range if provided.
    let byte_range = match copy_range {
        Some(ref range_str) => {
            match parse_copy_source_range(range_str, src_record.size) {
                Ok(r) => Some(r),
                Err(msg) => {
                    return s3_error_response(S3Error::with_message(
                        S3ErrorCode::InvalidArgument,
                        msg,
                        &resource,
                    ));
                }
            }
        }
        None => None,
    };

    // Stream source blob (optionally ranged) through get → put.
    let get_result = match state.blob.get(&src_record.blob_id, byte_range).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    let blob_id = BlobId::new();
    let put_result = match state.blob.put(&blob_id, get_result.stream).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Insert part record (returns old for cleanup).
    let part = arca_core::types::PartRecord {
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
    let now = chrono::Utc::now();

    // UploadPartCopy returns XML with ETag and LastModified.
    let xml = xml_types::copy_object_result(&put_result.etag, &now);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .header("ETag", &etag)
        .body(Body::from(xml))
        .expect("build upload_part_copy response")
}

/// Parses `x-amz-copy-source-range` header (format: `bytes=START-END`).
fn parse_copy_source_range(value: &str, file_size: u64) -> Result<ByteRange, &'static str> {
    let range_str = value
        .strip_prefix("bytes=")
        .ok_or("Invalid range format")?;
    let parts: Vec<&str> = range_str.splitn(2, '-').collect();
    if parts.len() != 2 {
        return Err("Invalid range format");
    }
    let start: u64 = parts[0].parse().map_err(|_| "Invalid range start")?;
    let end: u64 = parts[1].parse().map_err(|_| "Invalid range end")?;
    if start > end || start >= file_size {
        return Err("The requested range is not satisfiable");
    }
    let end = end.min(file_size - 1);
    Ok(ByteRange {
        start,
        end: Some(end),
    })
}

/// Parses the `x-amz-copy-source` header value into (bucket, key).
///
/// The header value may be URL-encoded and may contain a `?versionId=` suffix.
/// Format: `/bucket/key` or `bucket/key` (leading slash optional).
fn parse_copy_source(value: &str) -> Option<(String, String)> {
    let decoded = urlencoding::decode(value).ok()?;
    let decoded = decoded.as_ref();

    // Strip optional leading slash.
    let path = decoded.strip_prefix('/').unwrap_or(decoded);

    // Strip optional ?versionId= suffix.
    let path = path.split('?').next().unwrap_or(path);

    // Split into bucket/key.
    let slash_pos = path.find('/')?;
    let bucket = &path[..slash_pos];
    let key = &path[slash_pos + 1..];

    if bucket.is_empty() || key.is_empty() {
        return None;
    }

    Some((bucket.to_string(), key.to_string()))
}

/// GET /{bucket}/{*key} — GetObject
pub async fn get_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}/{key}");

    // Check bucket exists (S3 returns NoSuchBucket, not NoSuchKey).
    match state.metadata.head_bucket(&bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    let record = match state.metadata.get_object(&bucket, &key).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchKey, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    // Check conditional headers (If-Match, If-None-Match, etc.).
    if let Some(resp) =
        check_conditionals(request.headers(), &record.etag, &record.last_modified, true)
    {
        return resp;
    }

    let range = parse_range_header(request.headers(), record.size);

    let byte_range = match range {
        RangeParseResult::Range(r) => Some(r),
        RangeParseResult::None => None,
        RangeParseResult::Invalid => {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{}", record.size))
                .body(Body::empty())
                .expect("build 416 response");
        }
    };

    let (status, content_length, content_range) = match byte_range {
        Some(ref r) => {
            let end = r.end.unwrap_or(record.size - 1);
            let len = end - r.start + 1;
            let range_str = format!("bytes {}-{}/{}", r.start, end, record.size);
            (StatusCode::PARTIAL_CONTENT, len, Some(range_str))
        }
        None => (StatusCode::OK, record.size, None),
    };

    let get_result = match state.blob.get(&record.blob_id, byte_range).await {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, &resource),
    };

    let etag = format!("\"{}\"", record.etag);
    let last_modified = record.last_modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let content_type = record
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let mut builder = Response::builder()
        .status(status)
        .header("ETag", &etag)
        .header("Last-Modified", &last_modified)
        .header("Content-Length", content_length)
        .header("Content-Type", &content_type)
        .header("Accept-Ranges", "bytes");

    if let Some(range_str) = content_range {
        builder = builder.header("Content-Range", range_str);
    }

    builder
        .body(Body::from_stream(get_result.stream))
        .expect("build get_object response")
}

/// HEAD /{bucket}/{*key} — HeadObject
pub async fn head_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
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

    let record = match state.metadata.get_object(&bucket, &key).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchKey, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    // Check conditional headers (If-Match, If-None-Match, etc.).
    if let Some(resp) =
        check_conditionals(request.headers(), &record.etag, &record.last_modified, true)
    {
        return resp;
    }

    let etag = format!("\"{}\"", record.etag);
    let last_modified = record.last_modified.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    let content_type = record
        .content_type
        .unwrap_or_else(|| "application/octet-stream".to_string());

    Response::builder()
        .status(StatusCode::OK)
        .header("ETag", &etag)
        .header("Last-Modified", &last_modified)
        .header("Content-Length", record.size)
        .header("Content-Type", &content_type)
        .header("Accept-Ranges", "bytes")
        .body(Body::empty())
        .expect("build head_object response")
}

/// DELETE /{bucket}/{*key} — DeleteObject or AbortMultipartUpload
///
/// Dispatches based on query parameters:
/// - `?uploadId=X` → AbortMultipartUpload
/// - Otherwise → DeleteObject
pub async fn delete_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    // Check for AbortMultipartUpload (distinguished by query param).
    if let Some(query) = request.uri().query() {
        let params: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some((_, uid)) = params.iter().find(|(k, _)| k == "uploadId") {
            return super::multipart::abort_multipart_upload(
                state,
                bucket,
                key,
                uid.clone(),
            )
            .await;
        }
    }

    let resource = format!("/{bucket}/{key}");

    // Check bucket exists (S3 returns NoSuchBucket, not NoSuchKey).
    match state.metadata.head_bucket(&bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    let old = match state.metadata.delete_object(&bucket, &key).await {
        Ok(old) => old,
        Err(e) => return internal_error_response(e, &resource),
    };

    // Delete blob if record existed.
    if let Some(old_record) = old {
        if let Err(e) = state.blob.delete(&old_record.blob_id).await {
            tracing::warn!(error = %e, "Failed to delete blob for deleted object");
        }
    }

    // S3 returns 204 regardless of whether the object existed.
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .expect("build delete_object response")
}

/// POST /{bucket}/{*key} — CreateMultipartUpload or CompleteMultipartUpload
///
/// Dispatches based on query parameters:
/// - `?uploads` → CreateMultipartUpload
/// - `?uploadId=X` → CompleteMultipartUpload
pub async fn post_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
    if let Some(query) = request.uri().query() {
        // ?uploads → CreateMultipartUpload
        // Also handle ?uploads= (mc sends this form)
        if query == "uploads"
            || query == "uploads="
            || query.starts_with("uploads&")
            || query.starts_with("uploads=&")
            || query.contains("&uploads")
            || query.contains("&uploads=")
        {
            return super::multipart::create_multipart_upload(state, bucket, key, request).await;
        }

        // ?uploadId=X → CompleteMultipartUpload
        let params: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if let Some((_, uid)) = params.iter().find(|(k, _)| k == "uploadId") {
            return super::multipart::complete_multipart_upload(
                state,
                bucket,
                key,
                uid.clone(),
                request,
            )
            .await;
        }
    }

    crate::xml::error_response::not_implemented_response(&format!("/{bucket}/{key}"))
}

/// Result of parsing the Range header.
enum RangeParseResult {
    /// Valid range to apply.
    Range(ByteRange),
    /// No Range header present.
    None,
    /// Invalid or unsatisfiable range — should return 416.
    Invalid,
}

/// Parses the `Range` header into a `ByteRange`.
///
/// Supports `bytes=START-END`, `bytes=START-`, and `bytes=-N` (suffix) formats.
fn parse_range_header(headers: &http::HeaderMap, file_size: u64) -> RangeParseResult {
    let range_str = match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        Some(s) => s,
        None => return RangeParseResult::None,
    };
    let range_str = match range_str.strip_prefix("bytes=") {
        Some(s) => s,
        None => return RangeParseResult::Invalid,
    };

    let parts: Vec<&str> = range_str.splitn(2, '-').collect();
    if parts.len() != 2 {
        return RangeParseResult::Invalid;
    }

    if parts[0].is_empty() {
        // Suffix range: bytes=-N (last N bytes).
        let suffix_len: u64 = match parts[1].parse() {
            Ok(n) if n > 0 => n,
            _ => return RangeParseResult::Invalid,
        };
        if file_size == 0 {
            return RangeParseResult::Invalid;
        }
        let start = file_size.saturating_sub(suffix_len);
        return RangeParseResult::Range(ByteRange { start, end: None });
    }

    let start: u64 = match parts[0].parse() {
        Ok(n) => n,
        Err(_) => return RangeParseResult::Invalid,
    };
    let end: Option<u64> = if parts[1].is_empty() {
        None
    } else {
        match parts[1].parse() {
            Ok(n) => Some(n),
            Err(_) => return RangeParseResult::Invalid,
        }
    };

    // Clamp end to file size.
    let end = end.map(|e| e.min(file_size - 1));

    if start >= file_size {
        return RangeParseResult::Invalid;
    }

    RangeParseResult::Range(ByteRange { start, end })
}
