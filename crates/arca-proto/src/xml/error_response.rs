//! S3 XML error response helpers.

use axum::response::Response;
use http::StatusCode;

use arca_core::{ArcaError, S3Error, S3ErrorCode};

/// Marker extension placed on error responses so the request_id middleware
/// can replace the placeholder `<RequestId>` in the XML body with the real
/// middleware-assigned request ID.
#[derive(Clone, Debug)]
pub struct ErrorRequestId(pub String);

/// Converts an [`S3Error`] into an Axum HTTP response with XML body.
///
/// Stores the S3Error's placeholder request_id in a response extension so
/// the request_id middleware can swap it with the real one.
pub fn s3_error_response(err: S3Error) -> Response {
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let placeholder_id = err.request_id.clone();
    let body = err.to_xml();

    let mut builder = Response::builder()
        .status(status)
        .header("Content-Type", "application/xml");
    // Every retriable 503 carries a Retry-After hint (review M4): the cluster
    // quorum/syncing refusals flow through ServiceUnavailable, and well-behaved
    // clients honor the header instead of hammering. (SlowDown 503s come from
    // the rate-limit middleware, which sets its own.)
    if err.code == S3ErrorCode::ServiceUnavailable {
        builder = builder.header("Retry-After", "5");
    }
    let mut response = builder
        .body(axum::body::Body::from(body))
        .expect("build error response");
    response
        .extensions_mut()
        .insert(ErrorRequestId(placeholder_id));
    response
}

/// Returns a 501 NotImplemented S3 XML error response.
pub fn not_implemented_response(resource: &str) -> Response {
    let err = S3Error::new(S3ErrorCode::NotImplemented, resource);
    s3_error_response(err)
}

/// Converts an [`ArcaError`] into an appropriate HTTP response.
///
/// - `ArcaError::S3` errors are returned as S3 XML error responses.
/// - `ArcaError::Internal` errors are returned as 500 InternalError.
pub fn internal_error_response(err: ArcaError, resource: &str) -> Response {
    match err {
        ArcaError::S3(s3_err) => s3_error_response(s3_err),
        ArcaError::DecryptionFailed(msg) => {
            tracing::warn!(error = %msg, resource, "Decryption failed");
            let s3_err = S3Error::with_message(S3ErrorCode::AccessDenied, msg, resource);
            s3_error_response(s3_err)
        }
        ArcaError::Internal(msg) => {
            tracing::error!(error = %msg, resource, "Internal error");
            let s3_err = S3Error::new(S3ErrorCode::InternalError, resource);
            s3_error_response(s3_err)
        }
    }
}
