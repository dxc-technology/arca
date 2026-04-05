//! S3-compatible HTTP router with Admin API.

use std::time::Duration;

use axum::routing::{delete, get, post, put};
use axum::Router;
use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use http::{HeaderName, Method};
use tower_http::cors::{AllowHeaders, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, OnRequest, OnResponse, TraceLayer};
use tracing::Level;

use crate::handlers::{admin, admin_grants, admin_monitoring, admin_notifications, admin_presigned_urls, admin_settings, admin_teams, admin_users, archive, bucket, object};
use crate::middleware;
use crate::state::AppState;

/// Builds the Axum router with all S3 routes, Admin API routes, and middleware.
///
/// Layer order (outermost → innermost, i.e. request flows top-down):
///   RequestId → TraceLayer → Auth → [VirtualHost] → handlers
///
/// Admin routes live under `/admin/*` with their own auth middleware
/// (JSON errors instead of S3 XML). `/admin/health` is unauthenticated.
///
/// **NormalizeLayer must be applied outside the Router** (in main.rs)
/// because it needs to modify the URI *before* Axum routing.
/// Auth reads the original URI from request extensions (set by NormalizeLayer).
pub fn build_router(state: AppState) -> Router {
    let has_domain = state.domain.is_some();

    // --- S3 router ---
    let mut s3_app = Router::new()
        // Service-level: ListBuckets
        .route("/", get(bucket::list_buckets))
        // Bucket operations
        .route(
            "/{bucket}",
            get(bucket::get_bucket)
                .head(bucket::head_bucket)
                .put(bucket::create_bucket)
                .delete(bucket::delete_bucket)
                .post(bucket::post_bucket),
        )
        // Object operations
        .route(
            "/{bucket}/{*key}",
            get(object::get_object)
                .head(object::head_object)
                .put(object::put_object)
                .delete(object::delete_object)
                .post(object::post_object),
        );

    // Virtual-host rewrite runs innermost (closest to handlers).
    if has_domain {
        s3_app = s3_app.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::virtual_host::virtual_host_middleware,
        ));
    }

    // Per-credential rate limiting (after auth, so identity is available).
    s3_app = s3_app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::rate_limit::credential_rate_limit_middleware,
    ));

    // S3 auth middleware (returns S3 XML errors).
    s3_app = s3_app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::auth::auth_middleware,
    ));

    // --- Admin router (authenticated endpoints) ---
    let admin_auth = Router::new()
        .route("/info", get(admin::info))
        .route("/stats", get(admin::stats))
        .route("/me", get(admin_users::me))
        // Legacy credential endpoints (operate on calling user's credentials)
        .route(
            "/credentials",
            get(admin::list_credentials).post(admin::create_credential),
        )
        .route(
            "/credentials/{access_key_id}",
            put(admin::update_credential).delete(admin::delete_credential),
        )
        .route("/archive", post(archive::archive))
        .route("/presign", post(admin::presign))
        // User management
        .route("/users", get(admin_users::list_users).post(admin_users::create_user))
        .route(
            "/users/{user_id}",
            get(admin_users::get_user)
                .put(admin_users::update_user)
                .delete(admin_users::delete_user),
        )
        .route(
            "/users/{user_id}/credentials",
            get(admin_users::list_user_credentials).post(admin_users::create_user_credential),
        )
        .route(
            "/users/{user_id}/grants",
            get(admin_users::list_user_grants),
        )
        .route(
            "/users/{user_id}/grants/{grant_id}",
            put(admin_users::attach_user_grant).delete(admin_users::detach_user_grant),
        )
        .route(
            "/users/{user_id}/effective-grants",
            get(admin_users::effective_user_grants),
        )
        .route(
            "/users/{user_id}/teams",
            get(admin_users::list_user_teams),
        )
        // Team management
        .route("/teams", get(admin_teams::list_teams).post(admin_teams::create_team))
        .route(
            "/teams/{team_id}",
            get(admin_teams::get_team)
                .put(admin_teams::update_team)
                .delete(admin_teams::delete_team),
        )
        .route(
            "/teams/{team_id}/members",
            get(admin_teams::list_members),
        )
        .route(
            "/teams/{team_id}/members/{user_id}",
            put(admin_teams::add_member).delete(admin_teams::remove_member),
        )
        .route(
            "/teams/{team_id}/grants",
            get(admin_teams::list_team_grants),
        )
        .route(
            "/teams/{team_id}/grants/{grant_id}",
            put(admin_teams::attach_team_grant).delete(admin_teams::detach_team_grant),
        )
        // Instance settings
        .route("/settings", get(admin_settings::list_settings))
        .route(
            "/settings/{key}",
            put(admin_settings::update_setting).delete(admin_settings::delete_setting),
        )
        // Audit log and metrics history
        .route("/audit", get(admin_monitoring::list_audit).delete(admin_monitoring::clear_audit))
        .route("/audit/stats", get(admin_monitoring::audit_stats))
        .route("/metrics/history", get(admin_monitoring::metrics_history))
        // Notification events
        .route("/notifications/events", get(admin_notifications::list_notification_events).delete(admin_notifications::clear_notification_events))
        .route("/notifications/events/count", get(admin_notifications::count_notification_events))
        .route("/notifications/test-webhook", post(admin_notifications::test_webhook))
        .route("/notifications/test-connector", post(admin_notifications::test_connector))
        // Presigned URL tracking
        .route("/presigned-urls", get(admin_presigned_urls::list_presigned_urls))
        .route("/presigned-urls/{id}", delete(admin_presigned_urls::delete_presigned_url))
        // Grant management
        .route("/grants", get(admin_grants::list_grants).post(admin_grants::create_grant))
        .route(
            "/grants/{grant_id}",
            get(admin_grants::get_grant)
                .put(admin_grants::update_grant)
                .delete(admin_grants::delete_grant),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::admin_auth::admin_auth_middleware,
        ));

    // --- Admin router (public endpoints) ---
    let admin_public = Router::new()
        .route("/health", get(admin::health))
        .route("/metrics", get(admin::prometheus_metrics));

    // --- Combine admin routers ---
    let admin = Router::new().merge(admin_public).merge(admin_auth);

    // --- CORS layer ---
    // Allows the web console (running on a different origin) and third-party
    // clients to access both S3 and Admin API endpoints.
    let cors = CorsLayer::very_permissive()
        .allow_methods([
            Method::GET,
            Method::PUT,
            Method::POST,
            Method::DELETE,
            Method::HEAD,
            Method::OPTIONS,
        ])
        .allow_headers(AllowHeaders::list([
            AUTHORIZATION,
            CONTENT_TYPE,
            HeaderName::from_static("x-amz-content-sha256"),
            HeaderName::from_static("x-amz-date"),
            HeaderName::from_static("x-amz-copy-source"),
            HeaderName::from_static("x-amz-metadata-directive"),
            HeaderName::from_static("x-amz-server-side-encryption-customer-algorithm"),
            HeaderName::from_static("x-amz-server-side-encryption-customer-key"),
            HeaderName::from_static("x-amz-server-side-encryption-customer-key-md5"),
            HeaderName::from_static("x-amz-copy-source-server-side-encryption-customer-algorithm"),
            HeaderName::from_static("x-amz-copy-source-server-side-encryption-customer-key"),
            HeaderName::from_static("x-amz-copy-source-server-side-encryption-customer-key-md5"),
            HeaderName::from_static("x-amz-tagging"),
            HeaderName::from_static("x-amz-tagging-directive"),
            HeaderName::from_static("x-amz-object-lock-mode"),
            HeaderName::from_static("x-amz-object-lock-retain-until-date"),
            HeaderName::from_static("x-amz-object-lock-legal-hold-status"),
            HeaderName::from_static("x-amz-bypass-governance-retention"),
            HeaderName::from_static("x-amz-checksum-algorithm"),
            HeaderName::from_static("x-amz-checksum-sha256"),
            HeaderName::from_static("x-amz-checksum-crc32"),
            HeaderName::from_static("x-amz-checksum-crc32c"),
            HeaderName::from_static("x-amz-checksum-crc64nvme"),
            HeaderName::from_static("x-amz-storage-class"),
            HeaderName::from_static("x-amz-object-attributes"),
        ]))
        .expose_headers([
            ETAG,
            CONTENT_LENGTH,
            HeaderName::from_static("content-disposition"),
            HeaderName::from_static("x-amz-request-id"),
            HeaderName::from_static("x-amz-server-side-encryption"),
            HeaderName::from_static("x-amz-server-side-encryption-customer-algorithm"),
            HeaderName::from_static("x-amz-server-side-encryption-customer-key-md5"),
            HeaderName::from_static("x-amz-version-id"),
            HeaderName::from_static("x-amz-object-lock-mode"),
            HeaderName::from_static("x-amz-object-lock-retain-until-date"),
            HeaderName::from_static("x-amz-object-lock-legal-hold-status"),
            HeaderName::from_static("x-amz-checksum-sha256"),
            HeaderName::from_static("x-amz-checksum-crc32"),
            HeaderName::from_static("x-amz-checksum-crc32c"),
            HeaderName::from_static("x-amz-checksum-crc64nvme"),
            HeaderName::from_static("x-amz-storage-class"),
        ]);

    // --- Merge everything ---
    // Admin routes are nested under /admin, S3 routes at root.
    // Layer order (outermost → innermost):
    //   RequestId → Validate → IP RateLimit → Audit → Trace → CORS → Auth → [Credential RateLimit] → Handlers
    Router::new()
        .nest("/admin", admin)
        .merge(s3_app)
        .layer(cors)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
                .on_request(RequestLogger)
                .on_response(ResponseLogger),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::audit::audit_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::rate_limit::ip_rate_limit_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::validate::validate_middleware,
        ))
        .layer(axum::middleware::from_fn(
            middleware::request_id::request_id_middleware,
        ))
        .with_state(state)
}

#[derive(Clone)]
struct RequestLogger;

impl<B> OnRequest<B> for RequestLogger {
    fn on_request(&mut self, request: &http::Request<B>, _span: &tracing::Span) {
        tracing::info!(
            method = %request.method(),
            uri = %request.uri(),
            "request",
        );
    }
}

#[derive(Clone)]
struct ResponseLogger;

impl<B> OnResponse<B> for ResponseLogger {
    fn on_response(
        self,
        response: &http::Response<B>,
        latency: Duration,
        _span: &tracing::Span,
    ) {
        tracing::info!(
            status = response.status().as_u16(),
            latency_ms = latency.as_millis(),
            "response",
        );
    }
}
