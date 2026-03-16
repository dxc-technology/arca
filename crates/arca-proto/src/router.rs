//! S3-compatible HTTP router with Admin API.

use std::time::Duration;

use axum::routing::{delete, get, post};
use axum::Router;
use http::header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG};
use http::{HeaderName, Method};
use tower_http::cors::{AllowHeaders, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, OnRequest, OnResponse, TraceLayer};
use tracing::Level;

use crate::handlers::{admin, archive, bucket, object};
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

    // S3 auth middleware (returns S3 XML errors).
    s3_app = s3_app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::auth::auth_middleware,
    ));

    // --- Admin router (authenticated endpoints) ---
    let admin_auth = Router::new()
        .route("/info", get(admin::info))
        .route("/stats", get(admin::stats))
        .route(
            "/credentials",
            get(admin::list_credentials).post(admin::create_credential),
        )
        .route(
            "/credentials/{access_key_id}",
            delete(admin::delete_credential),
        )
        .route("/archive", post(archive::archive))
        .route("/presign", post(admin::presign))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::admin_auth::admin_auth_middleware,
        ));

    // --- Admin router (public endpoints) ---
    let admin_public = Router::new().route("/health", get(admin::health));

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
        ]))
        .expose_headers([
            ETAG,
            CONTENT_LENGTH,
            HeaderName::from_static("content-disposition"),
            HeaderName::from_static("x-amz-request-id"),
            HeaderName::from_static("x-amz-server-side-encryption"),
            HeaderName::from_static("x-amz-server-side-encryption-customer-algorithm"),
            HeaderName::from_static("x-amz-server-side-encryption-customer-key-md5"),
        ]);

    // --- Merge everything ---
    // Admin routes are nested under /admin, S3 routes at root.
    // Layer order (outermost → innermost):
    //   RequestId → Trace → CORS → Auth → Handlers
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
