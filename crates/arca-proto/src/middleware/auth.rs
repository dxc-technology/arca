//! AWS SigV4 authentication middleware.

use axum::extract::State;
use axum::response::Response;
use http::StatusCode;

use arca_auth::{
    parse_authorization, parse_query_string_auth, verify_presigned_request, verify_request,
    PresignedVerifyInput, VerifyInput,
};
use arca_core::policy::{self, Evaluation};
use arca_core::store::{GrantStore, UserStore};
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

        // Anti-replay: reject header-signed requests whose x-amz-date is
        // outside the freshness window. The signature covers x-amz-date, so an
        // attacker cannot re-date a captured request without breaking it.
        if !super::within_replay_window(&request_datetime, chrono::Utc::now()) {
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
    let identity = match resolve_identity(
        state.users.as_ref(),
        state.grants.as_ref(),
        credential,
    )
    .await
    {
        Ok(Some(id)) => id,
        Ok(None) => {
            // The credential is valid but its user no longer exists (e.g. a
            // dangling credential left after the user was deleted). Fail
            // closed: deny rather than synthesizing a root identity.
            return s3_error_response(S3Error::new(S3ErrorCode::AccessDenied, &uri_path));
        }
        Err(_) => {
            return Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(axum::body::Body::empty())
                .expect("build error response");
        }
    };

    // S3 authorization: evaluate policies for non-root users.
    if !identity.is_root() {
        let (action, resource) = determine_s3_action_resource(&method, &uri_path, &query_string);
        let result = policy::evaluate_grants(&identity.effective_policies, action, &resource);
        if !matches!(result, Evaluation::Allow) {
            return s3_error_response(S3Error::new(S3ErrorCode::AccessDenied, &uri_path));
        }
    }

    request.extensions_mut().insert(identity);
    next.run(request).await
}

/// Resolves a credential to a full identity with user and effective policies.
///
/// Returns `Ok(None)` when the credential's `user_id` does not resolve to an
/// existing user: the caller must fail closed (deny) rather than treat the
/// caller as root. `Err(())` is reserved for genuine backend errors.
///
/// Takes the two stores it needs rather than the whole [`AppState`], so the
/// fail-closed contract is unit-testable without a full server state.
async fn resolve_identity(
    users: &dyn UserStore,
    grants: &dyn GrantStore,
    credential: Credential,
) -> Result<Option<AuthenticatedIdentity>, ()> {
    let Some(user) = users
        .get_user(&credential.user_id)
        .await
        .map_err(|e| tracing::error!("Failed to get user: {e}"))?
    else {
        tracing::warn!(
            user_id = &credential.user_id,
            "Credential's user not found; denying (fail closed)"
        );
        return Ok(None);
    };

    let effective_policies = if user.is_root {
        // Root users bypass policy evaluation, no need to load policies.
        Vec::new()
    } else {
        grants
            .get_effective_policies(&user.user_id)
            .await
            .map_err(|e| tracing::error!("Failed to load effective policies: {e}"))?
    };

    Ok(Some(AuthenticatedIdentity {
        credential,
        user,
        effective_policies,
    }))
}

/// Determines the S3 action and resource ARN from the HTTP method, path, and query.
///
/// Returns `(action, resource)` where action is an `s3:*` constant and resource
/// is an ARN like `arn:aws:s3:::bucket` or `arn:aws:s3:::bucket/key`.
fn determine_s3_action_resource(method: &str, path: &str, query: &str) -> (&'static str, String) {
    use arca_core::policy::actions::*;

    // Parse path segments: /{bucket}/{key...}
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let (bucket, key) = match trimmed.split_once('/') {
        Some((b, k)) => (b, Some(k)),
        None => (trimmed, None),
    };

    // Service-level: GET /
    if bucket.is_empty() {
        return (S3_LIST_ALL_MY_BUCKETS, "*".to_string());
    }

    // Object-level operations (bucket + key present)
    if let Some(key) = key {
        if !key.is_empty() {
            let resource = format!("arn:aws:s3:::{bucket}/{key}");
            let action = match method {
                "GET" | "HEAD" => S3_GET_OBJECT,
                "PUT" => S3_PUT_OBJECT,
                "DELETE" => S3_DELETE_OBJECT,
                "POST" => {
                    if query.contains("uploads") {
                        S3_PUT_OBJECT // CreateMultipartUpload / CompleteMultipartUpload
                    } else {
                        S3_PUT_OBJECT
                    }
                }
                _ => S3_GET_OBJECT,
            };
            return (action, resource);
        }
    }

    // Bucket-level operations
    let resource = format!("arn:aws:s3:::{bucket}");
    let action = match method {
        "PUT" => {
            if query.contains("encryption") {
                S3_PUT_BUCKET_ENCRYPTION
            } else {
                S3_CREATE_BUCKET
            }
        }
        "GET" | "HEAD" => {
            if query.contains("encryption") {
                S3_GET_BUCKET_ENCRYPTION
            } else if query.contains("location") {
                S3_GET_BUCKET_LOCATION
            } else if method == "HEAD" {
                S3_LIST_BUCKET
            } else {
                S3_LIST_BUCKET
            }
        }
        "DELETE" => {
            if query.contains("encryption") {
                S3_DELETE_BUCKET_ENCRYPTION
            } else {
                S3_DELETE_BUCKET
            }
        }
        "POST" => {
            if query.contains("delete") {
                S3_DELETE_OBJECT
            } else {
                S3_LIST_BUCKET
            }
        }
        _ => S3_LIST_BUCKET,
    };

    (action, resource)
}

/// Parses an X-Amz-Date string (YYYYMMDDTHHMMSSZ) into a chrono DateTime.
fn parse_amz_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%dT%H%M%SZ")
        .ok()
        .map(|dt| dt.and_utc())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use arca_core::error::ArcaError;
    use arca_core::policy::PolicyDocument;
    use arca_core::types::{Grant, User};

    use super::*;

    /// How the stubbed [`UserStore`] answers `get_user`.
    enum UsersStub {
        /// The credential's user exists.
        Found(User),
        /// The credential is valid but its user is gone (dangling credential).
        Missing,
        /// The metadata backend is failing.
        Broken,
    }

    /// Stub that serves `get_user` only: `resolve_identity` must not reach for
    /// anything else on the user store, and a panic here would prove it does.
    #[async_trait::async_trait]
    impl UserStore for UsersStub {
        async fn get_user(&self, _user_id: &str) -> Result<Option<User>, ArcaError> {
            match self {
                UsersStub::Found(u) => Ok(Some(u.clone())),
                UsersStub::Missing => Ok(None),
                UsersStub::Broken => Err(ArcaError::Internal("metadata backend down".into())),
            }
        }

        async fn put_user(&self, _user: &User) -> Result<(), ArcaError> {
            unreachable!("resolve_identity must not write users")
        }
        async fn get_user_by_username(&self, _u: &str) -> Result<Option<User>, ArcaError> {
            unreachable!("resolve_identity resolves by user_id, not username")
        }
        async fn list_users(&self) -> Result<Vec<User>, ArcaError> {
            unreachable!("resolve_identity must not enumerate users")
        }
        async fn update_user(
            &self,
            _user_id: &str,
            _username: Option<&str>,
            _description: Option<&str>,
        ) -> Result<bool, ArcaError> {
            unreachable!("resolve_identity must not write users")
        }
        async fn delete_user(&self, _user_id: &str) -> Result<bool, ArcaError> {
            unreachable!("resolve_identity must not write users")
        }
    }

    /// Stub grant store that counts `get_effective_policies` calls, so a test
    /// can assert that root short-circuits the policy load entirely.
    #[derive(Default)]
    struct GrantsStub {
        calls: AtomicUsize,
        fail: bool,
    }

    impl GrantsStub {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl GrantStore for GrantsStub {
        async fn get_effective_policies(
            &self,
            _user_id: &str,
        ) -> Result<Vec<PolicyDocument>, ArcaError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(ArcaError::Internal("metadata backend down".into()));
            }
            Ok(Vec::new())
        }

        async fn put_grant(&self, _g: &Grant) -> Result<(), ArcaError> {
            unreachable!("resolve_identity must not write grants")
        }
        async fn get_grant(&self, _id: &str) -> Result<Option<Grant>, ArcaError> {
            unreachable!("resolve_identity loads effective policies in one query")
        }
        async fn get_grant_by_name(&self, _n: &str) -> Result<Option<Grant>, ArcaError> {
            unreachable!("resolve_identity loads effective policies in one query")
        }
        async fn list_grants(&self) -> Result<Vec<Grant>, ArcaError> {
            unreachable!("resolve_identity must not enumerate grants")
        }
        async fn update_grant(
            &self,
            _id: &str,
            _name: Option<&str>,
            _desc: Option<&str>,
            _doc: Option<&PolicyDocument>,
        ) -> Result<bool, ArcaError> {
            unreachable!("resolve_identity must not write grants")
        }
        async fn delete_grant(&self, _id: &str) -> Result<bool, ArcaError> {
            unreachable!("resolve_identity must not write grants")
        }
        async fn attach_to_user(&self, _u: &str, _g: &str) -> Result<(), ArcaError> {
            unreachable!("resolve_identity must not write grants")
        }
        async fn detach_from_user(&self, _u: &str, _g: &str) -> Result<bool, ArcaError> {
            unreachable!("resolve_identity must not write grants")
        }
        async fn attach_to_team(&self, _t: &str, _g: &str) -> Result<(), ArcaError> {
            unreachable!("resolve_identity must not write grants")
        }
        async fn detach_from_team(&self, _t: &str, _g: &str) -> Result<bool, ArcaError> {
            unreachable!("resolve_identity must not write grants")
        }
        async fn list_user_grants(&self, _u: &str) -> Result<Vec<Grant>, ArcaError> {
            unreachable!("resolve_identity uses get_effective_policies")
        }
        async fn list_team_grants(&self, _t: &str) -> Result<Vec<Grant>, ArcaError> {
            unreachable!("resolve_identity uses get_effective_policies")
        }
    }

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

    /// THE regression test for the fail-closed fix: a valid credential whose
    /// user no longer exists used to be promoted to a synthetic ROOT identity,
    /// silently granting superuser and bypassing policy evaluation. It must now
    /// resolve to `None` so the middleware denies the request.
    #[tokio::test]
    async fn missing_user_resolves_to_none_never_to_root() {
        let grants = GrantsStub::default();
        let resolved = resolve_identity(&UsersStub::Missing, &grants, credential("ghost"))
            .await
            .expect("a missing user is not a backend error");

        assert!(
            resolved.is_none(),
            "a dangling credential must fail closed, not synthesize an identity"
        );
        assert_eq!(
            grants.calls(),
            0,
            "policies must not be loaded for an unresolvable credential"
        );
    }

    /// A backend failure must stay distinguishable from a missing user: the
    /// middleware maps `Err` to 500 and `Ok(None)` to 403, and collapsing the
    /// two is what previously let a DB error become a root identity.
    #[tokio::test]
    async fn backend_error_is_an_error_not_a_denial() {
        let grants = GrantsStub::default();
        let resolved = resolve_identity(&UsersStub::Broken, &grants, credential("someone")).await;
        assert!(resolved.is_err(), "a backend failure must surface as Err");
    }

    /// A failure loading the policies is also an error, not an empty policy set
    /// (which would silently deny every request for a legitimate user).
    #[tokio::test]
    async fn policy_load_failure_is_an_error() {
        let grants = GrantsStub {
            calls: AtomicUsize::new(0),
            fail: true,
        };
        let resolved =
            resolve_identity(&UsersStub::Found(user("alice", false)), &grants, credential("alice"))
                .await;
        assert!(resolved.is_err());
    }

    #[tokio::test]
    async fn existing_non_root_user_resolves_with_its_policies_loaded() {
        let grants = GrantsStub::default();
        let identity =
            resolve_identity(&UsersStub::Found(user("alice", false)), &grants, credential("alice"))
                .await
                .unwrap()
                .expect("an existing user must resolve");

        assert_eq!(identity.user.user_id, "alice");
        assert!(!identity.is_root());
        assert_eq!(grants.calls(), 1, "non-root identities need their policies");
    }

    /// Root bypasses policy evaluation, so loading its policies would be wasted
    /// work on every single request.
    #[tokio::test]
    async fn root_user_skips_the_policy_load() {
        let grants = GrantsStub::default();
        let identity =
            resolve_identity(&UsersStub::Found(user("root", true)), &grants, credential("root"))
                .await
                .unwrap()
                .expect("root must resolve");

        assert!(identity.is_root());
        assert!(identity.effective_policies.is_empty());
        assert_eq!(grants.calls(), 0, "root must not hit the grant store");
    }
}
