//! S3-compatible HTTP router.

use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::handlers::{bucket, object};
use crate::state::AppState;

/// Builds the Axum router with all S3 routes and middleware.
pub fn build_router(state: AppState) -> Router {
    Router::new()
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
        )
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
