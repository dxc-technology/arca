//! S3 XML error response helpers.

use axum::response::Response;
use http::StatusCode;

use arca_core::{ArcaError, S3Error, S3ErrorCode};

/// Converts an [`S3Error`] into an Axum HTTP response with XML body.
pub fn s3_error_response(err: S3Error) -> Response {
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = err.to_xml();

    Response::builder()
        .status(status)
        .header("Content-Type", "application/xml")
        .body(axum::body::Body::from(body))
        .expect("build error response")
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
        ArcaError::Internal(msg) => {
            tracing::error!(error = %msg, resource, "Internal error");
            let s3_err = S3Error::new(S3ErrorCode::InternalError, resource);
            s3_error_response(s3_err)
        }
    }
}
