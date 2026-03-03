//! S3-compatible HTTP router.

use std::time::Duration;

use axum::routing::get;
use axum::Router;
use tower_http::trace::{DefaultMakeSpan, OnRequest, OnResponse, TraceLayer};
use tracing::Level;

use crate::handlers::{bucket, object};
use crate::middleware;
use crate::state::AppState;

/// Builds the Axum router with all S3 routes and middleware.
///
/// Layer order (outermost → innermost, i.e. request flows top-down):
///   TraceLayer → Auth → [VirtualHost] → handlers
///
/// **NormalizeLayer must be applied outside the Router** (in main.rs)
/// because it needs to modify the URI *before* Axum routing.
/// Auth reads the original URI from request extensions (set by NormalizeLayer).
pub fn build_router(state: AppState) -> Router {
    let has_domain = state.domain.is_some();

    let mut app = Router::new()
        // Service-level: ListBuckets
        .route("/", get(bucket::list_buckets))
        // Bucket operations
        .route(
            "/{bucket}",
            get(bucket::get_bucket)
                .head(bucket::head_bucket)
                .put(bucket::create_bucket)
                .delete(bucket::delete_bucket),
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
        app = app.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            middleware::virtual_host::virtual_host_middleware,
        ));
    }

    // Auth middleware verifies the original URI (saved by NormalizeLayer).
    app = app.layer(axum::middleware::from_fn_with_state(
        state.clone(),
        middleware::auth::auth_middleware,
    ));

    // TraceLayer is outermost on the Router — logs all requests including auth failures.
    app.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_request(RequestLogger)
            .on_response(ResponseLogger),
    )
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
