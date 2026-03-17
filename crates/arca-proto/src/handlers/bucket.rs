//! Bucket operation handlers.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::response::Response;
use http::StatusCode;

use arca_core::s3::xml_types;
use arca_core::s3::xml_types::{DeleteErrorEntry, DeletedEntry};
use arca_core::types::{ListBucketResultParams, ListBucketV1ResultParams, ListEntry, ObjectRecord};
use arca_core::{validate_bucket_name, S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::{
    internal_error_response, not_implemented_response, s3_error_response,
};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;

/// URL-encode a string for S3's encoding-type=url, preserving '/'.
///
/// S3 percent-encodes all characters except unreserved chars (RFC 3986:
/// `A-Z a-z 0-9 - _ . ~`) and the path separator `/`.
fn s3_url_encode(s: &str) -> String {
    s.split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// GET / — ListBuckets
pub async fn list_buckets(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Response {
    let resource = "/";
    let owner = request
        .extensions()
        .get::<crate::middleware::identity::AuthenticatedIdentity>()
        .map(|id| id.username().to_string())
        .unwrap_or_else(|| "root".to_string());

    match state.metadata.list_buckets().await {
        Ok(buckets) => {
            let xml = xml_types::list_all_my_buckets_result(&buckets, &owner);
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

    // GetBucketLocation: s3-tests and clients call ?location during setup.
    if params.iter().any(|(k, _)| k == "location") {
        // Check bucket exists first.
        match state.metadata.head_bucket(&bucket).await {
            Ok(Some(_)) => {
                let xml = xml_types::location_constraint();
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/xml")
                    .body(Body::from(xml))
                    .expect("build location response");
            }
            Ok(None) => {
                return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
            }
            Err(e) => return internal_error_response(e, &resource),
        }
    }

    // GetBucketEncryption
    if params.iter().any(|(k, _)| k == "encryption") {
        match state.metadata.head_bucket(&bucket).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
            }
            Err(e) => return internal_error_response(e, &resource),
        }
        // Check per-bucket config, then fall back to global default.
        let algo = match state.metadata.get_bucket_config(&bucket, "encryption_algorithm").await {
            Ok(Some(v)) => Some(v),
            Ok(None) => {
                if state.encryption_enabled {
                    Some("AES256".to_string())
                } else {
                    None
                }
            }
            Err(e) => return internal_error_response(e, &resource),
        };
        return match algo {
            Some(algorithm) => {
                let xml = format!(
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                    <ServerSideEncryptionConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                      <Rule>\
                        <ApplyServerSideEncryptionByDefault>\
                          <SSEAlgorithm>{algorithm}</SSEAlgorithm>\
                        </ApplyServerSideEncryptionByDefault>\
                        <BucketKeyEnabled>false</BucketKeyEnabled>\
                      </Rule>\
                    </ServerSideEncryptionConfiguration>"
                );
                Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", "application/xml")
                    .body(Body::from(xml))
                    .expect("build get_bucket_encryption response")
            }
            None => {
                s3_error_response(S3Error::new(
                    S3ErrorCode::ServerSideEncryptionConfigurationNotFoundError,
                    &resource,
                ))
            }
        };
    }

    // TECHDEBT(TD-007): Unimplemented GET bucket operations return 501.
    let unimplemented_get_ops = [
        "acl", "cors", "lifecycle", "logging", "notification",
        "policy", "replication", "tagging", "website", "object-lock",
        "ownershipControls", "publicAccessBlock", "policyStatus",
        "accelerate", "requestPayment", "inventory", "analytics",
        "metrics", "intelligenttiering",
    ];
    for op in &unimplemented_get_ops {
        if params.iter().any(|(k, _)| k == *op) {
            return not_implemented_response(&resource);
        }
    }

    // ListMultipartUploads: GET /{bucket}?uploads
    if params.iter().any(|(k, _)| k == "uploads") {
        return list_multipart_uploads(state, &bucket, &resource, &params).await;
    }

    // ListObjectVersions: mc sends ?versions= for recursive delete.
    // Since we don't support versioning, return current objects as Version entries.
    if params.iter().any(|(k, _)| k == "versions") {
        return list_object_versions(state, &bucket, &resource, &params).await;
    }

    // Dispatch based on list-type parameter.
    match get_param("list-type") {
        Some("2") => list_objects_v2(state, &bucket, &resource, &params).await,
        Some(_) => not_implemented_response(&resource),
        // No list-type: ListObjects V1 (used by older SDKs and s3-tests).
        None => list_objects_v1(state, &bucket, &resource, &params).await,
    }
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
    let delimiter = get_param("delimiter").filter(|d| !d.is_empty());
    let start_after = get_param("start-after");
    let continuation_token = get_param("continuation-token");
    let encoding_type = get_param("encoding-type");
    let fetch_owner = get_param("fetch-owner") == Some("true");

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

    // max-keys=0: return empty results with IsTruncated=false.
    if max_keys == 0 {
        let xml_params = ListBucketResultParams {
            name: bucket,
            prefix,
            delimiter,
            max_keys,
            is_truncated: false,
            key_count: 0,
            contents: &[],
            common_prefixes: &[],
            continuation_token,
            next_continuation_token: None,
            start_after,
            encoding_type,
            fetch_owner,
        };
        let xml = xml_types::list_bucket_result(&xml_params);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/xml")
            .body(Body::from(xml))
            .expect("build list_objects_v2 response");
    }

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

    // When delimiter is set, records collapse into CommonPrefixes, so we can't
    // simply fetch max_keys+1 raw records. Instead, we fetch in batches and
    // group until we have enough result items (contents + common_prefixes).
    //
    // Key subtleties:
    // - Multiple raw keys may collapse into one CommonPrefix (only counted once).
    // - When paginating, the continuation token may point into the middle of a
    //   CommonPrefix group. We must skip prefix groups already returned on a
    //   previous page by comparing against the effective_start_after value.
    // - IsTruncated should only be true if there are genuinely NEW result items
    //   beyond max_keys (not just duplicate keys in an already-seen prefix group).
    let (contents, common_prefixes, is_truncated, next_token) = if let Some(delim) = delimiter {
        let pfx = prefix.unwrap_or("");
        let mut all_contents = Vec::new();
        let mut all_prefixes = Vec::new();
        let mut seen_prefixes = std::collections::HashSet::new();
        let mut cursor = effective_start_after.map(|s| s.to_string());
        let skip_prefix_up_to = effective_start_after.map(|s| s.to_string());
        let mut truncated = false;
        let batch_size: u32 = (max_keys + 1).max(100);

        loop {
            let records = match state
                .metadata
                .list_objects(bucket, prefix, cursor.as_deref(), batch_size)
                .await
            {
                Ok(r) => r,
                Err(e) => return internal_error_response(e, resource),
            };
            let exhausted = (records.len() as u32) < batch_size;

            for record in &records {
                let after_prefix = &record.key[pfx.len()..];
                let common_prefix_opt = after_prefix.find(delim).map(|pos| {
                    format!("{}{}", pfx, &after_prefix[..pos + delim.len()])
                });

                // Skip prefix groups already returned on a previous page.
                if let Some(ref cp) = common_prefix_opt {
                    if let Some(ref skip) = skip_prefix_up_to {
                        if cp.as_str() <= skip.as_str() {
                            continue;
                        }
                    }
                }

                // Check if this record would produce a new result item.
                let is_new_item = match &common_prefix_opt {
                    Some(cp) => !seen_prefixes.contains(cp.as_str()),
                    None => true,
                };

                if is_new_item {
                    let result_count = all_contents.len() + all_prefixes.len();
                    if result_count as u32 >= max_keys {
                        truncated = true;
                        break;
                    }
                }

                if let Some(cp) = common_prefix_opt {
                    if seen_prefixes.insert(cp.clone()) {
                        all_prefixes.push(cp);
                    }
                } else {
                    all_contents.push(record_to_list_entry(record, fetch_owner));
                }
            }

            if truncated || exhausted {
                break;
            }
            cursor = records.last().map(|r| r.key.clone());
        }

        let token = if truncated {
            // Use the lexicographically greatest result (content key or
            // CommonPrefix). This ensures the next page's skip_prefix_up_to
            // correctly skips prefix groups already returned.
            let last_content = all_contents.last().map(|e| e.key.as_str());
            let last_prefix = all_prefixes.last().map(|p| p.as_str());
            let last_key = match (last_content, last_prefix) {
                (Some(c), Some(p)) => Some(std::cmp::max(c, p)),
                (Some(c), None) => Some(c),
                (None, Some(p)) => Some(p),
                (None, None) => None,
            };
            last_key.map(|k| BASE64.encode(k.as_bytes()))
        } else {
            None
        };

        (all_contents, all_prefixes, truncated, token)
    } else {
        // No delimiter: simple fetch.
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
        let contents: Vec<ListEntry> = records.iter().map(|r| record_to_list_entry(r, fetch_owner)).collect();
        let next_token = if is_truncated {
            records.last().map(|r| BASE64.encode(r.key.as_bytes()))
        } else {
            None
        };
        (contents, vec![], is_truncated, next_token)
    };

    let key_count = (contents.len() + common_prefixes.len()) as u32;

    // Apply URL encoding if requested. S3 encodes special characters but
    // preserves '/' (the path separator) in keys and CommonPrefixes.
    let (contents, common_prefixes) = if encoding_type == Some("url") {
        let enc_contents: Vec<ListEntry> = contents
            .into_iter()
            .map(|mut e| {
                e.key = s3_url_encode(&e.key);
                e
            })
            .collect();
        let enc_prefixes: Vec<String> = common_prefixes
            .into_iter()
            .map(|p| s3_url_encode(&p))
            .collect();
        (enc_contents, enc_prefixes)
    } else {
        (contents, common_prefixes)
    };

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
        encoding_type,
        fetch_owner,
    };

    let xml = xml_types::list_bucket_result(&xml_params);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build list_objects_v2 response")
}

/// Handles ListObjects V1 requests.
///
/// V1 uses `Marker`/`NextMarker` instead of `ContinuationToken`/`NextContinuationToken`.
async fn list_objects_v1(
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
    let delimiter = get_param("delimiter").filter(|d| !d.is_empty());
    let marker = get_param("marker");
    let encoding_type = get_param("encoding-type");

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

    // max-keys=0: return empty results with IsTruncated=false.
    if max_keys == 0 {
        let xml_params = ListBucketV1ResultParams {
            name: bucket,
            prefix,
            delimiter,
            marker,
            next_marker: None,
            max_keys,
            is_truncated: false,
            contents: &[],
            common_prefixes: &[],
            encoding_type,
        };
        let xml = xml_types::list_bucket_v1_result(&xml_params);
        return Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", "application/xml")
            .body(Body::from(xml))
            .expect("build list_objects_v1 response");
    }

    // Delimiter-aware fetch with proper grouping and pagination.
    // See list_objects_v2 for detailed comments on the algorithm.
    let (contents, common_prefixes, is_truncated, next_marker_owned) = if let Some(delim) =
        delimiter
    {
        let pfx = prefix.unwrap_or("");
        let mut all_contents = Vec::new();
        let mut all_prefixes = Vec::new();
        let mut seen_prefixes = std::collections::HashSet::new();
        let mut cursor = marker.map(|s| s.to_string());
        let skip_prefix_up_to = marker.map(|s| s.to_string());
        let mut truncated = false;
        let batch_size: u32 = (max_keys + 1).max(100);

        loop {
            let records = match state
                .metadata
                .list_objects(bucket, prefix, cursor.as_deref(), batch_size)
                .await
            {
                Ok(r) => r,
                Err(e) => return internal_error_response(e, resource),
            };
            let exhausted = (records.len() as u32) < batch_size;

            for record in &records {
                let after_prefix = &record.key[pfx.len()..];
                let common_prefix_opt = after_prefix.find(delim).map(|pos| {
                    format!("{}{}", pfx, &after_prefix[..pos + delim.len()])
                });

                // Skip prefix groups already returned on a previous page.
                if let Some(ref cp) = common_prefix_opt {
                    if let Some(ref skip) = skip_prefix_up_to {
                        if cp.as_str() <= skip.as_str() {
                            continue;
                        }
                    }
                }

                let is_new_item = match &common_prefix_opt {
                    Some(cp) => !seen_prefixes.contains(cp.as_str()),
                    None => true,
                };

                if is_new_item {
                    let result_count = all_contents.len() + all_prefixes.len();
                    if result_count as u32 >= max_keys {
                        truncated = true;
                        break;
                    }
                }

                if let Some(cp) = common_prefix_opt {
                    if seen_prefixes.insert(cp.clone()) {
                        all_prefixes.push(cp);
                    }
                } else {
                    all_contents.push(record_to_list_entry(record, false));
                }
            }

            if truncated || exhausted {
                break;
            }
            cursor = records.last().map(|r| r.key.clone());
        }

        let nm = if truncated {
            let last_content = all_contents.last().map(|e| e.key.as_str());
            let last_prefix = all_prefixes.last().map(|p| p.as_str());
            match (last_content, last_prefix) {
                (Some(c), Some(p)) => Some(std::cmp::max(c, p).to_string()),
                (Some(c), None) => Some(c.to_string()),
                (None, Some(p)) => Some(p.to_string()),
                (None, None) => None,
            }
        } else {
            None
        };

        (all_contents, all_prefixes, truncated, nm)
    } else {
        // No delimiter: simple fetch.
        let fetch_limit = max_keys + 1;
        let records = match state
            .metadata
            .list_objects(bucket, prefix, marker, fetch_limit)
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
        let contents: Vec<ListEntry> = records.iter().map(|r| record_to_list_entry(r, false)).collect();
        let nm = if is_truncated {
            records.last().map(|r| r.key.clone())
        } else {
            None
        };
        (contents, vec![], is_truncated, nm)
    };

    let next_marker = next_marker_owned.as_deref();

    // Apply URL encoding if requested.
    let (contents, common_prefixes) = if encoding_type == Some("url") {
        let enc_contents: Vec<ListEntry> = contents
            .into_iter()
            .map(|mut e| {
                e.key = s3_url_encode(&e.key);
                e
            })
            .collect();
        let enc_prefixes: Vec<String> = common_prefixes
            .into_iter()
            .map(|p| s3_url_encode(&p))
            .collect();
        (enc_contents, enc_prefixes)
    } else {
        (contents, common_prefixes)
    };

    let xml_params = ListBucketV1ResultParams {
        name: bucket,
        prefix,
        delimiter,
        marker,
        next_marker,
        max_keys,
        is_truncated,
        contents: &contents,
        common_prefixes: &common_prefixes,
        encoding_type,
    };

    let xml = xml_types::list_bucket_v1_result(&xml_params);
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build list_objects_v1 response")
}

/// Handles ListObjectVersions requests.
///
/// TECHDEBT(TD-003): Since Arca doesn't support versioning, each object is
/// returned as a single `<Version>` entry with `VersionId=null` and
/// `IsLatest=true`. This is enough for mc's `rm --recursive` workflow which
/// lists versions before batch-deleting.
async fn list_object_versions(
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
    let key_marker = get_param("key-marker");

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

    // Fetch max_keys + 1 to detect truncation.
    let fetch_limit = max_keys + 1;
    let records = match state
        .metadata
        .list_objects(bucket, prefix, key_marker, fetch_limit)
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

    // Build version entries (no delimiter grouping for versions API).
    let _ = delimiter; // Acknowledged but not used for version listing.
    let versions: Vec<ListEntry> = records.iter().map(|r| record_to_list_entry(r, false)).collect();

    let next_key_marker = if is_truncated {
        records.last().map(|r| r.key.as_str())
    } else {
        None
    };

    let xml = xml_types::list_versions_result(
        bucket,
        prefix,
        key_marker,
        max_keys,
        is_truncated,
        &versions,
        next_key_marker,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build list_object_versions response")
}

/// Handles ListMultipartUploads requests (`GET /{bucket}?uploads`).
async fn list_multipart_uploads(
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
    let key_marker = get_param("key-marker");
    let upload_id_marker = get_param("upload-id-marker");

    let max_uploads: u32 = match get_param("max-uploads") {
        Some(s) => match s.parse() {
            Ok(n) if n <= 1000 => n,
            Ok(_) => 1000,
            Err(_) => {
                return s3_error_response(S3Error::with_message(
                    S3ErrorCode::InvalidArgument,
                    "Invalid value for max-uploads",
                    resource,
                ));
            }
        },
        None => 1000,
    };

    let fetch_limit = max_uploads + 1;
    let uploads = match state
        .metadata
        .list_multipart_uploads(bucket, prefix, key_marker, upload_id_marker, fetch_limit)
        .await
    {
        Ok(r) => r,
        Err(e) => return internal_error_response(e, resource),
    };

    let is_truncated = uploads.len() as u32 > max_uploads;
    let uploads = if is_truncated {
        &uploads[..max_uploads as usize]
    } else {
        &uploads[..]
    };

    let (next_key_marker, next_upload_id_marker) = if is_truncated {
        uploads.last().map(|u| (u.key.as_str(), u.upload_id.as_str())).unzip()
    } else {
        (None, None)
    };

    let xml = xml_types::list_multipart_uploads_result(
        bucket,
        prefix,
        key_marker,
        upload_id_marker,
        max_uploads,
        is_truncated,
        uploads,
        next_key_marker,
        next_upload_id_marker,
        "root", // TODO(phase16): pass real owner from identity
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/xml")
        .body(Body::from(xml))
        .expect("build list_multipart_uploads response")
}

/// Handles PutBucketEncryption requests.
async fn put_bucket_encryption(
    state: AppState,
    bucket: &str,
    resource: &str,
    request: axum::extract::Request,
) -> Response {
    // Reject if no encryption key is configured on the server.
    if state.plain_blob.is_none() {
        return s3_error_response(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "Server-side encryption is not available: no master key configured in [encryption] section",
            resource,
        ));
    }

    // Check bucket exists.
    match state.metadata.head_bucket(bucket).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, resource));
        }
        Err(e) => return internal_error_response(e, resource),
    }

    // Read and parse XML body.
    let body_bytes = match axum::body::to_bytes(request.into_body(), 65_536).await {
        Ok(b) => b,
        Err(_) => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Request body too large or invalid",
                resource,
            ));
        }
    };
    let body_str = match std::str::from_utf8(&body_bytes) {
        Ok(s) => s,
        Err(_) => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "Request body is not valid UTF-8",
                resource,
            ));
        }
    };

    // Parse <SSEAlgorithm> from the XML body.
    // We accept only AES256 (the only algorithm Arca supports).
    let algorithm = parse_sse_algorithm(body_str);
    let algorithm = match algorithm {
        Some(algo) if algo == "AES256" => algo,
        Some(algo) => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                &format!("Unsupported SSE algorithm: {algo}"),
                resource,
            ));
        }
        None => {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::MalformedXML,
                "Missing SSEAlgorithm element",
                resource,
            ));
        }
    };

    match state
        .metadata
        .set_bucket_config(bucket, "encryption_algorithm", &algorithm)
        .await
    {
        Ok(()) => Response::builder()
            .status(StatusCode::OK)
            .body(Body::empty())
            .expect("build put_bucket_encryption response"),
        Err(e) => internal_error_response(e, resource),
    }
}

/// Parses the SSEAlgorithm value from a PutBucketEncryption XML body.
fn parse_sse_algorithm(xml: &str) -> Option<String> {
    // Simple extraction — look for <SSEAlgorithm>...</SSEAlgorithm>.
    let start_tag = "<SSEAlgorithm>";
    let end_tag = "</SSEAlgorithm>";
    let start = xml.find(start_tag)? + start_tag.len();
    let end = xml[start..].find(end_tag)? + start;
    Some(xml[start..end].trim().to_string())
}

/// Converts an `ObjectRecord` to a `ListEntry` for XML output.
fn record_to_list_entry(record: &ObjectRecord, fetch_owner: bool) -> ListEntry {
    let owner = if record.owner.is_empty() { "root" } else { &record.owner };
    ListEntry {
        key: record.key.clone(),
        last_modified: record.last_modified,
        etag: record.etag.clone(),
        size: record.size,
        storage_class: "STANDARD".to_string(), // TECHDEBT(TD-002): hardcoded storage class
        owner_id: if fetch_owner { Some(owner.to_string()) } else { None },
        owner_display_name: if fetch_owner { Some(owner.to_string()) } else { None },
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
            .header("x-amz-bucket-region", "us-east-1") // TECHDEBT(TD-004): hardcoded region
            .body(axum::body::Body::empty())
            .expect("build head bucket response"),
        Ok(None) => {
            let err = S3Error::new(S3ErrorCode::NoSuchBucket, &resource);
            s3_error_response(err)
        }
        Err(e) => internal_error_response(e, &resource),
    }
}

/// PUT /{bucket} — CreateBucket or other bucket-level PUT operations.
///
/// S3 overloads `PUT /{bucket}` with query parameters for versioning,
/// logging, lifecycle, etc. We dispatch unimplemented operations to 501.
pub async fn create_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}");
    let query = request.uri().query().unwrap_or("");

    // PutBucketEncryption
    if query.starts_with("encryption") || query.starts_with("encryption=") || query.starts_with("encryption&") {
        return put_bucket_encryption(state, &bucket, &resource, request).await;
    }

    // TECHDEBT(TD-007): Unimplemented bucket-level PUT operations return 501.
    let unimplemented_ops = [
        "versioning", "acl", "lifecycle", "cors", "logging",
        "notification", "policy", "replication", "tagging",
        "object-lock", "website", "accelerate",
        "requestPayment", "inventory", "analytics", "metrics",
        "ownershipControls", "publicAccessBlock", "intelligenttiering",
    ];
    for op in &unimplemented_ops {
        if query.starts_with(op) || query.starts_with(&format!("{op}=")) || query.starts_with(&format!("{op}&")) {
            return not_implemented_response(&resource);
        }
    }

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
        Err(arca_core::ArcaError::S3(ref s3err))
            if s3err.code == S3ErrorCode::BucketAlreadyOwnedByYou =>
        {
            // Idempotent: re-creating a bucket you own returns 200 (not 409).
            Response::builder()
                .status(StatusCode::OK)
                .header("Location", format!("/{bucket}"))
                .body(axum::body::Body::empty())
                .expect("build create bucket response")
        }
        Err(e) => internal_error_response(e, &resource),
    }
}

/// DELETE /{bucket} — DeleteBucket or DeleteBucketEncryption.
pub async fn delete_bucket(
    State(state): State<AppState>,
    Path(bucket): Path<String>,
    request: axum::extract::Request,
) -> Response {
    let resource = format!("/{bucket}");
    let query = request.uri().query().unwrap_or("");

    // DeleteBucketEncryption
    if query.starts_with("encryption") || query.starts_with("encryption=") || query.starts_with("encryption&") {
        match state.metadata.head_bucket(&bucket).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                return s3_error_response(S3Error::new(S3ErrorCode::NoSuchBucket, &resource));
            }
            Err(e) => return internal_error_response(e, &resource),
        }
        match state.metadata.delete_bucket_config(&bucket, "encryption_algorithm").await {
            Ok(_) => {
                return Response::builder()
                    .status(StatusCode::NO_CONTENT)
                    .body(Body::empty())
                    .expect("build delete_bucket_encryption response");
            }
            Err(e) => return internal_error_response(e, &resource),
        }
    }

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

    // S3 limits DeleteObjects to 1000 keys.
    if delete_body.objects.len() > 1000 {
        return s3_error_response(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "The delete request contained more than 1000 objects",
            &resource,
        ));
    }

    let quiet = delete_body.quiet;
    let mut deleted = Vec::new();
    let mut errors = Vec::new();

    for obj in &delete_body.objects {
        // Per-key conditional checks (ETag, LastModifiedTime, IfMatchSize).
        let has_condition = obj.etag.is_some()
            || obj.last_modified_time.is_some()
            || obj.size.is_some();
        if has_condition {
            match state.metadata.get_object(&bucket, &obj.key).await {
                Ok(Some(existing)) => {
                    let mut failed = false;

                    // ETag check.
                    if let Some(ref expected_etag) = obj.etag {
                        let quoted = format!("\"{}\"", existing.etag);
                        if expected_etag != &existing.etag
                            && expected_etag != &quoted
                            && expected_etag != "*"
                        {
                            failed = true;
                        }
                    }

                    // LastModifiedTime check.
                    // boto3 sends RFC 2822 format: "Sun, 09 Mar 2025 16:58:47 GMT".
                    // Compare at second precision since RFC 2822 has no sub-seconds.
                    if let Some(ref expected_time) = obj.last_modified_time {
                        let parsed = chrono::DateTime::parse_from_rfc2822(expected_time)
                            .or_else(|_| chrono::DateTime::parse_from_rfc3339(expected_time));
                        if let Ok(expected) = parsed {
                            let expected_secs = expected.timestamp();
                            let existing_secs = existing.last_modified.timestamp();
                            if expected_secs != existing_secs {
                                failed = true;
                            }
                        } else {
                            // Unknown format — treat as mismatch.
                            failed = true;
                        }
                    }

                    // Size check.
                    if let Some(ref expected_size) = obj.size {
                        if let Ok(size) = expected_size.parse::<u64>() {
                            if size != existing.size {
                                failed = true;
                            }
                        } else {
                            failed = true;
                        }
                    }

                    if failed {
                        errors.push(DeleteErrorEntry {
                            key: obj.key.clone(),
                            code: S3ErrorCode::PreconditionFailed.as_str().to_string(),
                            message: "At least one of the pre-conditions you specified did not hold.".to_string(),
                        });
                        continue;
                    }
                }
                Ok(None) => {
                    // Object doesn't exist — delete is a no-op, report success.
                    deleted.push(DeletedEntry {
                        key: obj.key.clone(),
                    });
                    continue;
                }
                Err(e) => {
                    tracing::error!(error = %e, key = %obj.key, "Error checking object for conditional delete");
                    errors.push(DeleteErrorEntry {
                        key: obj.key.clone(),
                        code: S3ErrorCode::InternalError.as_str().to_string(),
                        message: "We encountered an internal error. Please try again.".to_string(),
                    });
                    continue;
                }
            }
        }

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
                    code: S3ErrorCode::InternalError.as_str().to_string(),
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

