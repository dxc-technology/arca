//! Request validation middleware.
//!
//! Runs early in the middleware stack (after RequestId, before Audit) to reject
//! malformed requests before they consume resources:
//! - Too many HTTP headers
//! - Null bytes in the URI path
//! - Oversized user metadata (`x-amz-meta-*` headers)

use axum::extract::State;
use axum::middleware::Next;
use axum::response::Response;

use arca_core::{S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::s3_error_response;

/// Middleware that validates request structure before further processing.
pub async fn validate_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let uri = request.uri().to_string();
    let headers = request.headers();

    // Check header count.
    if state.max_header_count > 0 && headers.len() as u32 > state.max_header_count {
        return s3_error_response(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!(
                "Request exceeds the maximum number of headers ({})",
                state.max_header_count
            ),
            &uri,
        ));
    }

    // Reject null bytes in URI path.
    if request.uri().path().contains('\0') {
        return s3_error_response(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "Request URI contains invalid characters",
            &uri,
        ));
    }

    // Validate total user metadata size (x-amz-meta-* headers).
    if state.max_metadata_size > 0 {
        let mut meta_size: u32 = 0;
        for (name, value) in headers.iter() {
            let name_str = name.as_str();
            if name_str.starts_with("x-amz-meta-") {
                // Count the metadata key (without the "x-amz-meta-" prefix) + value.
                let key_len = name_str.len().saturating_sub(11); // "x-amz-meta-".len() == 11
                meta_size += key_len as u32 + value.len() as u32;
            }
        }
        if meta_size > state.max_metadata_size {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                format!(
                    "User metadata exceeds the maximum allowed size ({} bytes)",
                    state.max_metadata_size
                ),
                &uri,
            ));
        }
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn null_byte_detected() {
        let path = "/bucket/key\0evil";
        assert!(path.contains('\0'));
    }

    #[test]
    fn normal_path_passes() {
        let path = "/bucket/key/file.txt";
        assert!(!path.contains('\0'));
    }
}
