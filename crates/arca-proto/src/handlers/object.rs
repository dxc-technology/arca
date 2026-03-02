//! Object operation handlers.

use std::io;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use http::header;
use http::StatusCode;

use arca_core::store::{ByteRange, ByteStream, SidecarMeta};
use arca_core::types::{BlobId, ObjectRecord};
use arca_core::{S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::{internal_error_response, s3_error_response};

/// PUT /{bucket}/{*key} — PutObject
pub async fn put_object(
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
