//! Bucket operation handlers.

use axum::extract::{Path, State};
use axum::response::Response;
use http::StatusCode;

use arca_core::s3::xml_types;
use arca_core::{validate_bucket_name, S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::{internal_error_response, not_implemented_response, s3_error_response};

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

/// GET /{bucket} — ListObjectsV2 (not implemented yet, Phase 4)
pub async fn get_bucket(Path(bucket): Path<String>) -> Response {
    not_implemented_response(&format!("/{bucket}"))
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
