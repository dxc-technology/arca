//! AWS SigV4 authentication middleware for Admin API endpoints.
//!
//! Same verification logic as `auth.rs` but returns JSON error responses
//! instead of S3 XML. Supports both Authorization header and query-string
//! presigned URL auth.
//!
//! Two flavours:
//!
//! - [`admin_auth_middleware`]: SigV4 verify + admin grant check. Required
//!   for every endpoint that mutates server state or exposes data beyond the
//!   caller's own identity.
//! - [`admin_identity_middleware`]: SigV4 verify only. Any authenticated user
//!   passes. Used by `/admin/me` so that non-admin users can ask the server
//!   for their own username without holding `arca:ViewServerInfo`.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;

use arca_auth::{
    parse_authorization, parse_query_string_auth, verify_presigned_request, verify_request,
    PresignedVerifyInput, VerifyInput,
};
use arca_core::policy::{self, Evaluation};

use super::identity::AuthenticatedIdentity;
use crate::state::AppState;

/// Maximum presigned URL expiry: 7 days (604800 seconds), per AWS spec.
const MAX_PRESIGN_EXPIRES: u64 = 604_800;

/// Legacy wrapper kept for backward compatibility in handler signatures.
/// New code should use `AuthenticatedIdentity` from request extensions.
#[derive(Debug, Clone)]
pub struct AuthenticatedCredential(pub arca_core::types::Credential);

/// Result of authenticating an incoming request: identity plus the path that
/// was verified (so callers can do their own per-path grant checks without
/// re-extracting the URI).
struct AuthContext {
    identity: AuthenticatedIdentity,
    uri_path: String,
}

/// Verify SigV4 (header or presigned) and resolve the caller's identity.
///
/// Returns the populated [`AuthenticatedIdentity`] (plus the verified URI
/// path) and hands the request back to the caller so it can be passed on to
/// `next.run(...)`. On failure returns a ready JSON error response.
///
/// Note: `request` is taken by ownership so the helper does not hold a
/// reference across the `.await` points inside SigV4 verification — that
/// would make the resulting future `!Send` and break the Axum middleware
/// trait bounds.
async fn authenticate_request(
    state: &AppState,
    request: axum::extract::Request,
) -> Result<(AuthContext, axum::extract::Request), Response> {
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
                return Err(json_error(
                    StatusCode::FORBIDDEN,
                    "SignatureDoesNotMatch",
                    "The request signature we calculated does not match the signature you provided.",
                ));
            }
        };

        let cred = match state
            .credentials
            .get_credential(&parsed_auth.access_key_id)
            .await
        {
            Ok(Some(cred)) if cred.active => cred,
            Ok(Some(_)) | Ok(None) => {
                return Err(json_error(
                    StatusCode::FORBIDDEN,
                    "InvalidAccessKeyId",
                    "The AWS access key ID you provided does not exist in our records.",
                ));
            }
            Err(_) => {
                return Err(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    "Internal server error",
                ));
            }
        };

        if request_datetime.is_empty() {
            return Err(json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied"));
        }

        // Anti-replay: reject header-signed requests whose x-amz-date is
        // outside the freshness window (see auth.rs for rationale).
        if !super::within_replay_window(&request_datetime, chrono::Utc::now()) {
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "Request time too far from server time",
            ));
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
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            ));
        }

        cred
    } else if query_string.contains("X-Amz-Algorithm") {
        // --- Query-string presigned URL auth ---
        let parsed = match parse_query_string_auth(&query_string) {
            Ok(a) => a,
            Err(_) => {
                return Err(json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied"));
            }
        };

        if parsed.expires > MAX_PRESIGN_EXPIRES {
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "X-Amz-Expires must be less than a week (604800 seconds)",
            ));
        }

        if let Some(signed_at) = parse_amz_datetime(&parsed.request_datetime) {
            let expiry = signed_at + chrono::Duration::seconds(parsed.expires as i64);
            if chrono::Utc::now() > expiry {
                return Err(json_error(StatusCode::FORBIDDEN, "AccessDenied", "Request has expired"));
            }
        } else {
            return Err(json_error(StatusCode::FORBIDDEN, "AccessDenied", "Invalid date"));
        }

        let cred = match state
            .credentials
            .get_credential(&parsed.access_key_id)
            .await
        {
            Ok(Some(cred)) if cred.active => cred,
            Ok(Some(_)) | Ok(None) => {
                return Err(json_error(
                    StatusCode::FORBIDDEN,
                    "InvalidAccessKeyId",
                    "The AWS access key ID you provided does not exist in our records.",
                ));
            }
            Err(_) => {
                return Err(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    "Internal server error",
                ));
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
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            ));
        }

        cred
    } else {
        return Err(json_error(StatusCode::FORBIDDEN, "AccessDenied", "Access Denied"));
    };

    // Resolve identity: credential -> user -> effective policies.
    // Fail closed: a valid credential whose user no longer exists is denied,
    // and a backend error is a 500 — neither may be silently promoted to root.
    let user = match state.users.get_user(&credential.user_id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return Err(json_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "Access Denied",
            ));
        }
        Err(_) => {
            return Err(json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "InternalError",
                "Internal server error",
            ));
        }
    };

    let is_root = user.is_root;

    // Load effective policies for non-root users.
    let effective_policies = if is_root {
        Vec::new()
    } else {
        match state.grants.get_effective_policies(&user.user_id).await {
            Ok(p) => p,
            Err(_) => {
                return Err(json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InternalError",
                    "Failed to load user policies",
                ));
            }
        }
    };

    let identity = AuthenticatedIdentity {
        credential,
        user,
        effective_policies,
    };
    Ok((AuthContext { identity, uri_path }, request))
}

/// Whether this identity may reach the admin endpoint at `uri_path`.
///
/// Root users pass by identity. Everyone else must hold a grant satisfying the
/// per-path admin action (see [`determine_admin_action`]).
///
/// Privileges are a property of the user, never of the credential: this mirrors
/// S3 authorization (`middleware::auth`) and [`crate::authorize`], which have
/// always short-circuited on root and evaluated grants for everyone else.
fn admin_access_allowed(identity: &AuthenticatedIdentity, uri_path: &str) -> bool {
    if identity.user.is_root {
        return true;
    }
    let admin_action = determine_admin_action(uri_path);
    matches!(
        policy::evaluate_grants(&identity.effective_policies, admin_action, "*"),
        Evaluation::Allow
    )
}

/// Axum middleware: verify SigV4 + require admin access.
///
/// See [`admin_access_allowed`] for the authorization rule.
pub async fn admin_auth_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let (AuthContext { identity, uri_path }, mut request) =
        match authenticate_request(&state, request).await {
            Ok(ok) => ok,
            Err(resp) => return resp,
        };

    if !admin_access_allowed(&identity, &uri_path) {
        return json_error(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "Admin privileges required",
        );
    }

    // Store both for backward compat (existing handlers read AuthenticatedCredential).
    request
        .extensions_mut()
        .insert(AuthenticatedCredential(identity.credential.clone()));
    request.extensions_mut().insert(identity);
    next.run(request).await
}

/// Axum middleware: verify SigV4, no grant check.
///
/// Any authenticated user passes — non-admin users included. Used for the
/// `/admin/me` self-introspection endpoint so a regular S3 user with only
/// `s3:*` grants can still ask the server for their own username.
pub async fn admin_identity_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let (AuthContext { identity, .. }, mut request) =
        match authenticate_request(&state, request).await {
            Ok(ok) => ok,
            Err(resp) => return resp,
        };
    request
        .extensions_mut()
        .insert(AuthenticatedCredential(identity.credential.clone()));
    request.extensions_mut().insert(identity);
    next.run(request).await
}

/// Maps an admin API path to the required arca:* action.
fn determine_admin_action(path: &str) -> &'static str {
    use arca_core::policy::actions::*;

    // Strip the /admin/ prefix for matching.
    let sub = path.strip_prefix("/admin/").unwrap_or(path);

    if sub.starts_with("users") {
        ARCA_MANAGE_USERS
    } else if sub.starts_with("teams") {
        ARCA_MANAGE_TEAMS
    } else if sub.starts_with("grants") {
        ARCA_MANAGE_GRANTS
    } else if sub.starts_with("credentials") {
        ARCA_MANAGE_CREDENTIALS
    } else if sub == "info" || sub == "stats" {
        ARCA_VIEW_SERVER_INFO
    } else if sub.starts_with("presign") {
        ARCA_CREATE_PRESIGNED_URL
    } else if sub.starts_with("archive") {
        ARCA_CREATE_ARCHIVE
    } else {
        // Unknown admin path, require full admin access.
        "arca:*"
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use arca_core::policy::PolicyDocument;
    use arca_core::types::{Credential, User};

    fn user(user_id: &str, is_root: bool) -> User {
        User {
            user_id: user_id.to_string(),
            username: user_id.to_string(),
            description: String::new(),
            is_root,
            created_at: chrono::Utc::now(),
        }
    }

    fn credential(user_id: &str) -> Credential {
        Credential {
            access_key_id: "AKIATEST".to_string(),
            secret_access_key: "secret".to_string(),
            description: String::new(),
            created_at: chrono::Utc::now(),
            active: true,
            user_id: user_id.to_string(),
        }
    }

    fn identity(user_id: &str, is_root: bool, policies: Vec<PolicyDocument>) -> AuthenticatedIdentity {
        AuthenticatedIdentity {
            credential: credential(user_id),
            user: user(user_id, is_root),
            effective_policies: policies,
        }
    }

    fn policy(action: &str) -> PolicyDocument {
        serde_json::from_str(&format!(
            r#"{{"Version":"2012-10-17","Statement":[{{"Effect":"Allow","Action":["{action}"],"Resource":["*"]}}]}}"#
        ))
        .expect("valid policy document")
    }

    /// THE regression test for this fix: admin access follows the USER, not the
    /// credential. Every credential minted for root through
    /// `POST /admin/users/{id}/credentials` used to be born unusable — owned by
    /// root (so grants were never evaluated) yet carrying the pre-RBAC
    /// `admin=false` flag (so the gate denied it), leaving the holder locked out
    /// of every `/admin/*` route while the console showed them as a plain user.
    #[test]
    fn any_root_credential_reaches_the_admin_api() {
        let id = identity("root", true, Vec::new());
        assert!(admin_access_allowed(&id, "/admin/credentials"));
        assert!(admin_access_allowed(&id, "/admin/users"));
        assert!(admin_access_allowed(&id, "/admin/settings"));
    }

    #[test]
    fn non_root_without_grants_is_denied() {
        let id = identity("alice", false, Vec::new());
        assert!(!admin_access_allowed(&id, "/admin/credentials"));
    }

    #[test]
    fn non_root_with_wildcard_grant_is_allowed() {
        let id = identity("alice", false, vec![policy("*")]);
        assert!(admin_access_allowed(&id, "/admin/credentials"));
    }

    #[test]
    fn non_root_grant_must_match_the_path_action() {
        // A grant for one admin action does not open the others: the gate keys
        // on the action `determine_admin_action` derives from the path. Both
        // paths below map to a specific action, not to the `arca:*` catch-all
        // that unmapped admin paths fall back to.
        let action = determine_admin_action("/admin/credentials");
        let other = determine_admin_action("/admin/users");
        assert_ne!(action, other, "test needs two distinct admin actions");

        let id = identity("alice", false, vec![policy(action)]);
        assert!(admin_access_allowed(&id, "/admin/credentials"));
        assert!(!admin_access_allowed(&id, "/admin/users"));
    }
}
