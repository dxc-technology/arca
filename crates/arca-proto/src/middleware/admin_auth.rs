//! AWS SigV4 authentication middleware for Admin API endpoints.
//!
//! Same verification logic as `auth.rs` but returns JSON error responses
//! instead of S3 XML. Supports both Authorization header and query-string
//! presigned URL auth.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;

use arca_auth::{
    parse_authorization, parse_query_string_auth, verify_presigned_request, verify_request,
    PresignedVerifyInput, VerifyInput,
};
use arca_core::types::Credential;

use crate::state::AppState;

/// Maximum presigned URL expiry: 7 days (604800 seconds), per AWS spec.
const MAX_PRESIGN_EXPIRES: u64 = 604_800;

/// Wrapper for the authenticated credential, stored in request extensions.
#[derive(Debug, Clone)]
pub struct AuthenticatedCredential(pub Credential);

/// Axum middleware that verifies AWS SigV4 signatures, returning JSON errors.
pub async fn admin_auth_middleware(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Extract URI path and query string from the ORIGINAL URI.
    let original_uri = request
        .extensions()
        .get::<crate::middleware::normalize::OriginalUri>()
        .map(|u| u.0.clone())
        .unwrap_or_else(|| request.uri().clone());
    let uri_path = original_uri.path().to_string();
    let query_string = original_uri.query().unwrap_or("").to_string();
    let method = request.method().as_str().to_string();

    // Collect headers (shared by both auth paths).
    let mut headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| {
            (name.as_str().to_string(), crate::header_value_to_string(value))
        })
        .collect();

    // HTTP/2: synthesize host from :authority pseudo-header.
    if !headers.iter().any(|(n, _)| n == "host") {
        if let Some(authority) = request.uri().authority() {
            headers.push(("host".to_string(), authority.as_str().to_string()));
        }
    }

    let auth_header = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let request_datetime = request
        .headers()
        .get("x-amz-date")
        .or_else(|| request.headers().get(http::header::DATE))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let payload_hash = request
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD")
        .to_string();

    // Resolve credential via header auth or query-string auth.
    let credential = if let Some(auth_header) = auth_header {
        // --- Standard Authorization header auth ---
        let parsed_auth = match parse_authorization(&auth_header) {
            Ok(a) => a,
            Err(_) => {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "SignatureDoesNotMatch",
                    "The request signature we calculated does not match the signature you provided.",
                );
            }
        };

        let cred = match state
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
                );
            }
            Err(_) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    "Internal server error",
                );
            }
        };

        if request_datetime.is_empty() {
            return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
        }

        let input = VerifyInput {
            method: &method,
            uri_path: &uri_path,
            query_string: &query_string,
            headers: &headers,
            payload_hash: &payload_hash,
            auth: &parsed_auth,
            secret_access_key: &cred.secret_access_key,
            request_datetime: &request_datetime,
        };

        if verify_request(&input).is_err() {
            return json_error(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            );
        }

        cred
    } else if query_string.contains("X-Amz-Algorithm") {
        // --- Query-string presigned URL auth ---
        let parsed = match parse_query_string_auth(&query_string) {
            Ok(a) => a,
            Err(_) => {
                return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
            }
        };

        if parsed.expires > MAX_PRESIGN_EXPIRES {
            return json_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "X-Amz-Expires must be less than a week (604800 seconds)",
            );
        }

        if let Some(signed_at) = parse_amz_datetime(&parsed.request_datetime) {
            let expiry = signed_at + chrono::Duration::seconds(parsed.expires as i64);
            if chrono::Utc::now() > expiry {
                return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Request has expired");
            }
        } else {
            return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Invalid date");
        }

        let cred = match state
            .credentials
            .get_credential(&parsed.access_key_id)
            .await
        {
            Ok(Some(cred)) if cred.active => cred,
            Ok(Some(_)) | Ok(None) => {
                return json_error(
                    StatusCode::FORBIDDEN,
                    "InvalidAccessKeyId",
                    "The AWS access key ID you provided does not exist in our records.",
                );
            }
            Err(_) => {
                return json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    "Internal server error",
                );
            }
        };

        let input = PresignedVerifyInput {
            method: &method,
            uri_path: &uri_path,
            query_string: &query_string,
            headers: &headers,
            auth: &parsed,
            secret_access_key: &cred.secret_access_key,
        };

        if verify_presigned_request(&input).is_err() {
            return json_error(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            );
        }

        cred
    } else {
        return json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied");
    };

    // Require admin privilege for admin API endpoints.
    if !credential.admin {
        return json_error(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Admin privileges required",
        );
    }

    // Signature valid + admin — store credential and proceed.
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

/// Parses an X-Amz-Date string (YYYYMMDDTHHMMSSZ) into a chrono DateTime.
fn parse_amz_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|dt| dt.and_utc())
}
