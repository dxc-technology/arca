//! AWS SigV4 authentication middleware.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;

use arca_auth::{
    parse_authorization, parse_query_string_auth, verify_presigned_request, verify_request,
    PresignedVerifyInput, VerifyInput,
};
use arca_core::types::Credential;
use arca_core::{S3Error, S3ErrorCode};

use super::identity::AuthenticatedIdentity;
use crate::state::AppState;
use crate::xml::error_response::s3_error_response;

/// Maximum presigned URL expiry: 7 days (604800 seconds), per AWS spec.
const MAX_PRESIGN_EXPIRES: u64 = 604_800;

/// Axum middleware that verifies AWS SigV4 signatures on every request.
///
/// Supports both Authorization header auth and query-string presigned URL auth.
/// After successful auth, resolves the user identity and preloads effective
/// policies, storing `AuthenticatedIdentity` in request extensions.
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // Extract URI path and query string from the ORIGINAL URI
    // (before NormalizePathLayer strips trailing slashes).
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

    // Try Authorization header first, then query-string auth.
    let auth_header = request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    // Extract datetime from headers (used by header auth path).
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

    let credential = if let Some(auth_header) = auth_header {
        // --- Standard Authorization header auth ---
        let parsed_auth = match parse_authorization(&auth_header) {
            Ok(a) => a,
            Err(_) => {
                return s3_error_response(S3Error::new(
                    S3ErrorCode::SignatureDoesNotMatch,
                    &uri_path,
                ));
            }
        };

        let cred = match state
            .credentials
            .get_credential(&parsed_auth.access_key_id)
            .await
        {
            Ok(Some(cred)) if cred.active => cred,
            Ok(Some(_)) => {
                return s3_error_response(S3Error::new(
                    S3ErrorCode::InvalidAccessKeyId,
                    &uri_path,
                ));
            }
            Ok(None) => {
                return s3_error_response(S3Error::new(
                    S3ErrorCode::InvalidAccessKeyId,
                    &uri_path,
                ));
            }
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(axum::body::Body::empty())
                    .expect("build error response");
            }
        };

        if request_datetime.is_empty() {
            return s3_error_response(S3Error::new(S3ErrorCode::AccessDenied, &uri_path));
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
            return s3_error_response(S3Error::new(
                S3ErrorCode::SignatureDoesNotMatch,
                &uri_path,
            ));
        }

        cred
    } else if query_string.contains("X-Amz-Algorithm") {
        // --- Query-string presigned URL auth ---
        let parsed = match parse_query_string_auth(&query_string) {
            Ok(a) => a,
            Err(_) => {
                return s3_error_response(S3Error::new(S3ErrorCode::AccessDenied, &uri_path));
            }
        };

        // Validate X-Amz-Expires <= 7 days.
        if parsed.expires > MAX_PRESIGN_EXPIRES {
            return s3_error_response(S3Error::with_message(
                S3ErrorCode::AccessDenied,
                format!(
                    "X-Amz-Expires must be less than a week (604800 seconds), got {}",
                    parsed.expires
                ),
                &uri_path,
            ));
        }

        // Validate expiration: parse X-Amz-Date and check now < signed_at + expires.
        if let Some(signed_at) = parse_amz_datetime(&parsed.request_datetime) {
            let expiry = signed_at + chrono::Duration::seconds(parsed.expires as i64);
            if chrono::Utc::now() > expiry {
                return s3_error_response(S3Error::with_message(
                    S3ErrorCode::AccessDenied,
                    "Request has expired",
                    &uri_path,
                ));
            }
        } else {
            return s3_error_response(S3Error::new(S3ErrorCode::AccessDenied, &uri_path));
        }

        // Look up credential.
        let cred = match state
            .credentials
            .get_credential(&parsed.access_key_id)
            .await
        {
            Ok(Some(cred)) if cred.active => cred,
            Ok(Some(_)) | Ok(None) => {
                return s3_error_response(S3Error::new(
                    S3ErrorCode::InvalidAccessKeyId,
                    &uri_path,
                ));
            }
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(axum::body::Body::empty())
                    .expect("build error response");
            }
        };

        // Verify signature.
        let input = PresignedVerifyInput {
            method: &method,
            uri_path: &uri_path,
            query_string: &query_string,
            headers: &headers,
            auth: &parsed,
            secret_access_key: &cred.secret_access_key,
        };

        if verify_presigned_request(&input).is_err() {
            return s3_error_response(S3Error::new(
                S3ErrorCode::SignatureDoesNotMatch,
                &uri_path,
            ));
        }

        cred
    } else {
        // No auth at all.
        return s3_error_response(S3Error::new(S3ErrorCode::AccessDenied, &uri_path));
    };

    // Resolve identity: credential -> user -> effective policies.
    match resolve_identity(&state, credential).await {
        Ok(identity) => {
            request.extensions_mut().insert(identity);
            next.run(request).await
        }
        Err(_) => {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::empty())
                .expect("build error response")
        }
    }
}

/// Resolves a credential to a full identity with user and effective policies.
async fn resolve_identity(
    state: &AppState,
    credential: Credential,
) -> Result<AuthenticatedIdentity, ()> {
    let user = state
        .users
        .get_user(&credential.user_id)
        .await
        .map_err(|e| tracing::error!("Failed to get user: {e}"))?
        .unwrap_or_else(|| {
            // Fallback for credentials without a valid user (should not happen
            // after migration, but be defensive).
            tracing::warn!(
                user_id = &credential.user_id,
                "Credential's user not found, using synthetic root"
            );
            arca_core::types::User {
                user_id: credential.user_id.clone(),
                username: credential.user_id.clone(),
                description: String::new(),
                is_root: true,
                created_at: chrono::Utc::now(),
            }
        });

    let effective_policies = if user.is_root {
        // Root users bypass policy evaluation, no need to load policies.
        Vec::new()
    } else {
        state
            .grants
            .get_effective_policies(&user.user_id)
            .await
            .map_err(|e| tracing::error!("Failed to load effective policies: {e}"))?
    };

    Ok(AuthenticatedIdentity {
        credential,
        user,
        effective_policies,
    })
}

/// Parses an X-Amz-Date string (YYYYMMDDTHHMMSSZ) into a chrono DateTime.
fn parse_amz_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|dt| dt.and_utc())
}
