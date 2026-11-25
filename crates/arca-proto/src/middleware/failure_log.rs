//! Access log for 5xx responses, at the level their cause deserves.
//!
//! A 5xx is not always a fault: a node that is syncing after a restart, a
//! cluster without write quorum, a maintenance drain or a client over its rate
//! limit all answer 5xx on purpose. The code that produces such an answer marks
//! the response with a [`FailureClass`] extension (`s3_error_response`,
//! `AdminError`, the health and inter-node handlers); this middleware logs every
//! 5xx with its method and path at the level of that class:
//!
//! | Class        | Level | Examples                                              |
//! |--------------|-------|-------------------------------------------------------|
//! | `Unexpected` | ERROR | 500 InternalError, any 5xx nobody marked              |
//! | `Notable`    | WARN  | a peer not acknowledging a write, a 502 from a peer   |
//! | `Expected`   | DEBUG | syncing, no write quorum, drain, SlowDown, 501        |
//!
//! An unmarked 5xx is `Unexpected` by construction: a forgotten marker can only
//! add noise, never hide a fault. `Expected` refusals stay at DEBUG because the
//! owner of the state logs each episode once, with the count of refused
//! requests (the cluster state logger in `arca-server`), instead of one line
//! per request.
//!
//! Only the path is logged, never the query string: presigned URLs carry their
//! signature there.
//!
//! Cost on the hot path: this runs for every request, so it allocates nothing
//! unless the response is a 5xx (the `Uri` clone only bumps a reference count;
//! the path and the request id, read from the response headers set by the
//! request-id layer inside this one, are extracted only for a 5xx).

use std::time::Instant;

use arca_core::FailureClass;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

pub async fn failure_log_middleware(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let started = Instant::now();

    let response = next.run(request).await;

    let status = response.status();
    if status.is_server_error() {
        let class = response
            .extensions()
            .get::<FailureClass>()
            .copied()
            .unwrap_or_default();
        let latency_ms = started.elapsed().as_millis();
        let path = uri.path();
        let request_id = response
            .headers()
            .get("x-amz-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-");
        let status = status.as_u16();
        match level_for(class) {
            tracing::Level::ERROR => tracing::error!(
                status, %method, path, latency_ms, request_id, "request failed"
            ),
            tracing::Level::WARN => tracing::warn!(
                status, %method, path, latency_ms, request_id, "request refused"
            ),
            _ => tracing::debug!(
                status, %method, path, latency_ms, request_id, "request refused (expected state)"
            ),
        }
    }
    response
}

/// The level a 5xx of this class is logged at (the table above).
pub fn level_for(class: FailureClass) -> tracing::Level {
    match class {
        FailureClass::Unexpected => tracing::Level::ERROR,
        FailureClass::Notable => tracing::Level::WARN,
        FailureClass::Expected => tracing::Level::DEBUG,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmarked_failures_are_errors() {
        assert_eq!(FailureClass::default(), FailureClass::Unexpected);
        assert_eq!(level_for(FailureClass::default()), tracing::Level::ERROR);
    }

    #[test]
    fn classes_map_to_their_levels() {
        assert_eq!(level_for(FailureClass::Notable), tracing::Level::WARN);
        assert_eq!(level_for(FailureClass::Expected), tracing::Level::DEBUG);
    }
}
