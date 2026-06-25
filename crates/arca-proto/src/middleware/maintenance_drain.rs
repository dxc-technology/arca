//! S3 maintenance-drain middleware (Phase 30).
//!
//! When a maintenance-mode job is active on this node, the S3 data API is
//! drained: every S3 request is refused with `503 ServiceUnavailable`
//! (+ `Retry-After`) so EXTERNAL clients — not just the load balancer's health
//! probe — stop reading and writing for the duration of the operation. The
//! admin API and the maintenance worker stay live (this layer is applied to the
//! S3 router only).
//!
//! This is distinct from the graceful-shutdown drain (`AppState::draining`),
//! which is intentionally NOT hard-blocking: shutdown lets in-flight requests
//! finish while the load balancer drains via the health check. Maintenance mode
//! instead needs a hard quiescence on the node, so it refuses S3 outright.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;

use arca_core::{S3Error, S3ErrorCode};

use crate::state::AppState;
use crate::xml::error_response::s3_error_response;

pub async fn maintenance_drain_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if *state.maintenance_draining.borrow() {
        let resource = request.uri().path().to_string();
        return s3_error_response(S3Error::new(S3ErrorCode::ServiceUnavailable, &resource));
    }
    next.run(request).await
}
