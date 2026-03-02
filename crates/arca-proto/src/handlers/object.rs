//! Object operation handlers.

use std::io;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use http::header;
use http::StatusCode;

use arca_core::s3::xml_types;
use arca_core::store::{ByteRange, ByteStream, SidecarMeta};
use arca_core::types::{BlobId, ObjectRecord};
use arca_core::{S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::{internal_error_response, s3_error_response};

/// PUT /{bucket}/{*key} — PutObject or CopyObject
///
/// If the `x-amz-copy-source` header is present, this dispatches to `copy_object`.
/// Otherwise, it handles a normal PutObject.
pub async fn put_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
    request: axum::extract::Request,
) -> Response {
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

    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let body = request.into_body();
    let stream = body_to_byte_stream(body);

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

    let record = match state.metadata.get_object(&bucket, &key).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchKey, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

    let range = parse_range_header(request.headers(), record.size);

    let (status, content_length, content_range) = match range {
        Some(ref r) => {
            let end = r.end.unwrap_or(record.size - 1);
            let len = end - r.start + 1;
            let range_str = format!("bytes {}-{}/{}", r.start, end, record.size);
            (StatusCode::PARTIAL_CONTENT, len, Some(range_str))
        }
        None => (StatusCode::OK, record.size, None),
    };

    let get_result = match state.blob.get(&record.blob_id, range).await {
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
) -> Response {
    let resource = format!("/{bucket}/{key}");

    let record = match state.metadata.get_object(&bucket, &key).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchKey, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    };

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

/// DELETE /{bucket}/{*key} — DeleteObject
pub async fn delete_object(
    State(state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    let resource = format!("/{bucket}/{key}");

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

/// POST /{bucket}/{*key} — Not implemented yet (multipart).
pub async fn post_object(
    State(_state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    crate::xml::error_response::not_implemented_response(&format!("/{bucket}/{key}"))
}

/// Converts an Axum body into a `ByteStream`.
fn body_to_byte_stream(body: Body) -> ByteStream {
    use tokio_stream::StreamExt;

    let stream = body.into_data_stream();
    let mapped = stream.map(|result| {
        result.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    });
    Box::pin(mapped)
}

/// Parses the `Range` header into a `ByteRange`.
///
/// Supports `bytes=START-END` and `bytes=START-` formats only.
fn parse_range_header(headers: &http::HeaderMap, file_size: u64) -> Option<ByteRange> {
    let range_str = headers.get(header::RANGE)?.to_str().ok()?;
    let range_str = range_str.strip_prefix("bytes=")?;

    let parts: Vec<&str> = range_str.splitn(2, '-').collect();
    if parts.len() != 2 {
        return None;
    }

    let start: u64 = parts[0].parse().ok()?;
    let end: Option<u64> = if parts[1].is_empty() {
        None
    } else {
        Some(parts[1].parse().ok()?)
    };

    // Clamp end to file size.
    let end = end.map(|e| e.min(file_size - 1));

    if start >= file_size {
        return None;
    }

    Some(ByteRange { start, end })
}
