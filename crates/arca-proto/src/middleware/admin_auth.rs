//! AWS SigV4 authentication middleware for Admin API endpoints.
//!
//! Same verification logic as `auth.rs` but returns JSON error responses
//! instead of S3 XML.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;

use arca_auth::{parse_authorization, verify_request, VerifyInput};
use arca_core::types::Credential;

use crate::state::AppState;

/// Wrapper for the authenticated credential, stored in request extensions.
#[derive(Debug, Clone)]
pub struct AuthenticatedCredential(pub Credential);

/// Axum middleware that verifies AWS SigV4 signatures, returning JSON errors.
pub async fn admin_auth_middleware(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // 1. Extract Authorization header
    let auth_header = match request.headers().get(http::header::AUTHORIZATION) {
        Some(val) => match val.to_str() {
            Ok(s) => s.to_string(),
            Err(_) => return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied"),
        },
        None => return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied"),
    };

    // 2. Parse the Authorization header
    let parsed_auth = match parse_authorization(&auth_header) {
        Ok(a) => a,
        Err(_) => {
            return json_error(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            )
        }
    };

    // 3. Look up the credential
    let credential = match state
        .credentials
        .get_credential(&parsed_auth.access_key_id)
        .await
    {
        Ok(Some(cred)) if cred.active => cred,
        Ok(Some(_)) | Ok(None) => {
            return json_error(
                StatusCode::FORBIDDEN,
                "InvalidAccessKeyId",
                "The AWS access key ID you provided does not exist in our records.",
            )
        }
        Err(_) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                "Internal server error",
            )
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
        return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
    }

    // 5. Extract x-amz-content-sha256 (default to UNSIGNED-PAYLOAD)
    let payload_hash = request
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD")
        .to_string();

    // 6. Collect headers as (name, value) pairs.
    //    Non-ASCII bytes decoded as UTF-8 (see auth.rs for rationale).
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (name.as_str().to_string(), crate::header_value_to_string(value))
        })
        .collect();

    // 7. Extract URI path and query string from the ORIGINAL URI
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

    if verify_request(&input).is_err() {
        return json_error(
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
        );
    }

    // 9. Require admin privilege for admin API endpoints
    if !credential.admin {
        return json_error(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Admin privileges required",
        );
    }

    // 10. Signature valid + admin — store credential and proceed
    request
        .extensions_mut()
        .insert(AuthenticatedCredential(credential));
    next.run(request).await
}

/// Builds a JSON error response.
fn json_error(status: StatusCode, error: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": error,
        "message": message,
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("build JSON error response")
}
