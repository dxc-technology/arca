//! Bucket operation handlers.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use http::StatusCode;

use arca_core::s3::xml_types;
use arca_core::s3::xml_types::{DeleteErrorEntry, DeletedEntry};
use arca_core::types::{ListBucketResultParams, ListEntry, ObjectRecord};
use arca_core::{validate_bucket_name, S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::{
    internal_error_response, not_implemented_response, s3_error_response,
};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// GET / — ListBuckets
pub async fn list_buckets(State(state): State<AppState>) -> Response {
    let resource = "/";
    match state.metadata.list_buckets().await {
        Ok(buckets) => {
            let xml = xml_types::list_all_my_buckets_result(&buckets);
            Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", "application/xml")
                .body(axum::body::Body::from(xml))
                .expect("build list buckets response")
        }
        Err(e) => internal_error_response(e, resource),
    }
}

/// GET /{bucket} — ListObjectsV2 (dispatched when `list-type=2`) or 501.
pub async fn get_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}");

    // Parse query string manually (S3 uses hyphenated param names).
    let query = request.uri().query().unwrap_or("");
    let params: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

    let get_param = |name: &str| -> Option<&str> {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    // Only handle ListObjectsV2 (list-type=2). V1 returns 501.
    match get_param("list-type") {
        Some("2") => {}
        Some(_) => return not_implemented_response(&resource),
        None => {
            // No list-type param at all — treat as V2 (boto3 sends list-type=2,
            // but aws cli without --query may not; be lenient).
            // Actually, aws CLI always sends list-type=2 for `aws s3 ls`.
            // For strict S3 compat, we could 501 here, but being lenient is safer.
        }
    }

    list_objects_v2(state, &bucket, &resource, &params).await
}

/// Handles ListObjectsV2 requests.
async fn list_objects_v2(
    state: AppState,
    bucket: &str,
    resource: &str,
    params: &[(String, String)],
) -> Response {
    let get_param = |name: &str| -> Option<&str> {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };

    // Check bucket exists.
    match state.metadata.head_bucket(bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, resource));
        }
        Err(e) => return internal_error_response(e, resource),
    }

    let prefix = get_param("prefix");
    let delimiter = get_param("delimiter");
    let start_after = get_param("start-after");
    let continuation_token = get_param("continuation-token");

    let max_keys: u32 = match get_param("max-keys") {
        Some(s) => match s.parse() {
            Ok(n) if n <= 1000 => n,
            Ok(_) => 1000,
            Err(_) => {
                return s3_error_response(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "Invalid value for max-keys",
                    resource,
                ));
            }
        },
        None => 1000,
    };

    // Decode continuation token → use as start_after.
    let decoded_token: Option<String> = match continuation_token {
        Some(token) => match BASE64.decode(token) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(_) => {
                    return s3_error_response(S3Error::with_message(
                        S3ErrorCode::InvalidArgument,
                        "Invalid continuation token",
                        resource,
                    ));
                }
            },
            Err(_) => {
                return s3_error_response(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "Invalid continuation token",
                    resource,
                ));
            }
        },
        None => None,
    };

    // Effective start_after: continuation token takes priority over start-after.
    let effective_start_after = decoded_token.as_deref().or(start_after);

    // Fetch max_keys + 1 to detect truncation.
    let fetch_limit = max_keys + 1;
    let records = match state
        .metadata
        .list_objects(bucket, prefix, effective_start_after, fetch_limit)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, resource),
    };

    let is_truncated = records.len() as u32 > max_keys;
    let records = if is_truncated {
        &records[..max_keys as usize]
    } else {
        &records[..]
    };

    // Extract common prefixes if delimiter is set.
    let (contents, common_prefixes) = match delimiter {
        Some(delim) => extract_common_prefixes(records, prefix.unwrap_or(""), delim),
        None => (records.iter().map(record_to_list_entry).collect(), vec![]),
    };

    // Build next continuation token from last key.
    let next_token = if is_truncated {
        records.last().map(|r| BASE64.encode(r.key.as_bytes()))
    } else {
        None
    };

    let key_count = (contents.len() + common_prefixes.len()) as u32;

    let xml_params = ListBucketResultParams {
        name: bucket,
        prefix,
        delimiter,
        max_keys,
        is_truncated,
        key_count,
        contents: &contents,
        common_prefixes: &common_prefixes,
        continuation_token,
        next_continuation_token: next_token.as_deref(),
        start_after,
        encoding_type: None,
    };

    let xml = xml_types::list_bucket_result(&xml_params);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build list_objects_v2 response")
}

/// Separates records into direct contents and common prefix groups based on delimiter.
///
/// A common prefix is the portion of the key up to and including the first delimiter
/// occurrence after the prefix. Records that match a common prefix are grouped rather
/// than listed individually.
fn extract_common_prefixes(
    records: &[ObjectRecord],
    prefix: &str,
    delimiter: &str,
) -> (Vec<ListEntry>, Vec<String>) {
    let mut contents = Vec::new();
    let mut prefixes = Vec::new();
    let mut seen_prefixes = std::collections::HashSet::new();

    for record in records {
        let after_prefix = &record.key[prefix.len()..];
        if let Some(pos) = after_prefix.find(delimiter) {
            // This key has a delimiter after the prefix → common prefix.
            let common_prefix = format!("{}{}", prefix, &after_prefix[..pos + delimiter.len()]);
            if seen_prefixes.insert(common_prefix.clone()) {
                prefixes.push(common_prefix);
            }
        } else {
            // No delimiter after prefix → direct content.
            contents.push(record_to_list_entry(record));
        }
    }

    (contents, prefixes)
}

/// Converts an `ObjectRecord` to a `ListEntry` for XML output.
fn record_to_list_entry(record: &ObjectRecord) -> ListEntry {
    ListEntry {
        key: record.key.clone(),
        last_modified: record.last_modified,
        etag: record.etag.clone(),
        size: record.size,
        storage_class: "STANDARD".to_string(),
    }
}

/// HEAD /{bucket} — HeadBucket
pub async fn head_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
) -> Response {
    let resource = format!("/{bucket}");
    match state.metadata.head_bucket(&bucket).await {
        Ok(Some(_)) => Response::builder()
            .status(StatusCode::OK)
            .header("x-amz-bucket-region", "us-east-1")
            .body(axum::body::Body::empty())
            .expect("build head bucket response"),
        Ok(None) => {
            let err = S3Error::new(S3ErrorCode::NoSuchBucket, &resource);
            s3_error_response(err)
        }
        Err(e) => internal_error_response(e, &resource),
    }
}

/// PUT /{bucket} — CreateBucket
pub async fn create_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
) -> Response {
    let resource = format!("/{bucket}");

    // Validate bucket name
    if let Err(e) = validate_bucket_name(&bucket) {
        return s3_error_response(e);
    }

    match state.metadata.create_bucket(&bucket).await {
        Ok(()) => Response::builder()
            .status(StatusCode::OK)
            .header("Location", format!("/{bucket}"))
            .body(axum::body::Body::empty())
            .expect("build create bucket response"),
        Err(e) => internal_error_response(e, &resource),
    }
}

/// DELETE /{bucket} — DeleteBucket
pub async fn delete_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
) -> Response {
    let resource = format!("/{bucket}");

    // Check bucket exists.
    match state.metadata.head_bucket(&bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    // Check bucket is empty.
    match state.metadata.bucket_is_empty(&bucket).await {
        Ok(true) => {}
        Ok(false) => {
            return s3_error_response(S3Error::new(S3ErrorCode::BucketNotEmpty, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
    }

    match state.metadata.delete_bucket(&bucket).await {
        Ok(true) => Response::builder()
            .status(StatusCode::NO_CONTENT)
            .body(axum::body::Body::empty())
            .expect("build delete bucket response"),
        Ok(false) => {
            let err = S3Error::new(S3ErrorCode::NoSuchBucket, &resource);
            s3_error_response(err)
        }
        Err(e) => internal_error_response(e, &resource),
    }
}

/// POST /{bucket} — DeleteObjects (dispatched when `?delete` is present).
pub async fn post_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let query = request.uri().query().unwrap_or("");

    // Check for ?delete or ?delete= (mc sends ?delete=).
    let is_delete = query == "delete"
        || query == "delete="
        || query.starts_with("delete&")
        || query.starts_with("delete=&")
        || query.contains("&delete")
        || query.contains("&delete=");

    if !is_delete {
        return not_implemented_response(&format!("/{bucket}"));
    }

    delete_objects(state, bucket, request).await
}

/// Handles the DeleteObjects (`POST /{bucket}?delete`) operation.
async fn delete_objects(
    state: AppState,
    bucket: String,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}");

    // Check bucket exists.
    match state.metadata.head_bucket(&bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
        }
        Err(e) => return internal_error_response(e, &resource),
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

    let delete_body = match xml_types::parse_delete_objects(body_str) {
        Ok(b) => b,
        Err(_) => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Invalid Delete XML",
                &resource,
            ));
        }
    };

    let quiet = delete_body.quiet;
    let mut deleted = Vec::new();
    let mut errors = Vec::new();

    for obj in &delete_body.objects {
        match state.metadata.delete_object(&bucket, &obj.key).await {
            Ok(old) => {
                // Delete blob if record existed.
                if let Some(old_record) = old {
                    if let Err(e) = state.blob.delete(&old_record.blob_id).await {
                        tracing::warn!(
                            error = %e,
                            key = %obj.key,
                            "Failed to delete blob for deleted object"
                        );
                    }
                }
                // S3 reports success even if the key didn't exist.
                deleted.push(DeletedEntry {
                    key: obj.key.clone(),
                });
            }
            Err(e) => {
                tracing::error!(error = %e, key = %obj.key, "Error deleting object");
                errors.push(DeleteErrorEntry {
                    key: obj.key.clone(),
                    code: "InternalError".to_string(),
                    message: "We encountered an internal error. Please try again.".to_string(),
                });
            }
        }
    }

    let xml = xml_types::delete_objects_result(&deleted, &errors, quiet);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build delete_objects response")
}
