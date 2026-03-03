//! AWS SigV4 authentication middleware.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;

use arca_auth::{parse_authorization, verify_request, VerifyInput};
use arca_core::{S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::s3_error_response;

/// Axum middleware that verifies AWS SigV4 signatures on every request.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // 1. Extract Authorization header
    let auth_header = match request.headers().get(http::header::AUTHORIZATION) {
        Some(val) => match val.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => {
                return s3_error_response(S3Error::new(
                    S3ErrorCode::AccessDenied,
                    request.uri().path(),
                ));
            }
        },
        None => {
            return s3_error_response(S3Error::new(
                S3ErrorCode::AccessDenied,
                request.uri().path(),
            ));
        }
    };

    // 2. Parse the Authorization header
    let parsed_auth = match parse_authorization(&auth_header) {
        Ok(a) => a,
        Err(_) => {
            return s3_error_response(S3Error::new(
                S3ErrorCode::SignatureDoesNotMatch,
                request.uri().path(),
            ));
        }
    };

    // 3. Look up the credential
    let credential = match state
        .credentials
        .get_credential(&parsed_auth.access_key_id)
        .await
    {
        Ok(Some(cred)) if cred.active => cred,
        Ok(Some(_)) => {
            // Credential exists but is inactive
            return s3_error_response(S3Error::new(
                S3ErrorCode::InvalidAccessKeyId,
                request.uri().path(),
            ));
        }
        Ok(None) => {
            return s3_error_response(S3Error::new(
                S3ErrorCode::InvalidAccessKeyId,
                request.uri().path(),
            ));
        }
        Err(_) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::empty())
                .expect("build error response");
        }
    };

    // 4. Extract x-amz-date (or fall back to Date header)
    let request_datetime = request
        .headers()
        .get("x-amz-date")
        .or_else(|| request.headers().get(http::header::DATE))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if request_datetime.is_empty() {
        return s3_error_response(S3Error::new(
            S3ErrorCode::AccessDenied,
            request.uri().path(),
        ));
    }

    // 5. Extract x-amz-content-sha256 (default to UNSIGNED-PAYLOAD)
    let payload_hash = request
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD")
        .to_string();

    // 6. Collect headers as (name, value) pairs
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                value.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();

    // 7. Extract URI path and query string from the ORIGINAL URI
    //    (before NormalizePathLayer strips trailing slashes).
    let original_uri = request
        .extensions()
        .get::<crate::middleware::normalize::OriginalUri>()
        .map(|u| u.0.clone())
        .unwrap_or_else(|| request.uri().clone());
    let uri_path = original_uri.path().to_string();
    let query_string = original_uri.query().unwrap_or("").to_string();
    let method = request.method().as_str().to_string();

    // 8. Verify the signature
    let input = VerifyInput {
        method: &method,
        uri_path: &uri_path,
        query_string: &query_string,
        headers: &headers,
        payload_hash: &payload_hash,
        auth: &parsed_auth,
        secret_access_key: &credential.secret_access_key,
        request_datetime: &request_datetime,
    };

    if let Err(_) = verify_request(&input) {
        return s3_error_response(S3Error::new(
            S3ErrorCode::SignatureDoesNotMatch,
            &uri_path,
        ));
    }

    // 9. Signature valid — proceed to the next handler
    next.run(request).await
}
