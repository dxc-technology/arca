//! Rate limiting middleware.
//!
//! Provides per-IP and per-credential rate limiting using the GCRA algorithm
//! via the `governor` crate. Disabled when rate = 0 in config.

use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;

use axum::extract::ConnectInfo;
use axum::middleware::Next;
use axum::response::Response;
use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use http::StatusCode;

use arca_core::{S3Error, S3ErrorCode};

use crate::middleware::identity::AuthenticatedIdentity;
use crate::state::AppState;
use crate::xml::error_response::s3_error_response;

/// Keyed rate limiter type (keyed by String for IP or credential ID).
pub type KeyedRateLimiter =
    RateLimiter<String, DashMapStateStore<String>, DefaultClock>;

/// Creates a keyed rate limiter with the given per-second rate and burst capacity.
/// Returns `None` if rate is 0 (disabled).
pub fn create_rate_limiter(per_second: u32, burst: u32) -> Option<Arc<KeyedRateLimiter>> {
    let rate = NonZeroU32::new(per_second)?;
    let burst = NonZeroU32::new(burst.max(per_second))?; // burst >= rate
    let quota = Quota::per_second(rate).allow_burst(burst);
    Some(Arc::new(RateLimiter::dashmap(quota)))
}

/// Per-IP rate limiting middleware.
///
/// Extracts the client IP from `ConnectInfo` or `X-Forwarded-For` header and
/// checks against the per-IP rate limiter. Returns 503 SlowDown if exceeded.
pub async fn ip_rate_limit_middleware(
    state: axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(ref limiter) = state.ip_rate_limiter {
        let ip = extract_client_ip(&request);
        if limiter.check_key(&ip).is_err() {
            let uri = request.uri().to_string();
            return slow_down_response(&uri);
        }
    }
    next.run(request).await
}

/// Per-credential rate limiting middleware.
///
/// Extracts the access key ID from `AuthenticatedIdentity` (set by auth middleware)
/// and checks against the per-credential rate limiter. Returns 503 SlowDown if exceeded.
pub async fn credential_rate_limit_middleware(
    state: axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(ref limiter) = state.credential_rate_limiter {
        if let Some(identity) = request.extensions().get::<AuthenticatedIdentity>() {
            let key = identity.credential.access_key_id.clone();
            if limiter.check_key(&key).is_err() {
                let uri = request.uri().to_string();
                return slow_down_response(&uri);
            }
        }
    }
    next.run(request).await
}

/// Extracts the client IP address from `X-Forwarded-For` header or `ConnectInfo`.
fn extract_client_ip(request: &axum::extract::Request) -> String {
    // Check X-Forwarded-For first (for reverse proxy setups).
    if let Some(xff) = request.headers().get("x-forwarded-for") {
        if let Ok(val) = xff.to_str() {
            if let Some(first_ip) = val.split(',').next() {
                return first_ip.trim().to_string();
            }
        }
    }

    // Fall back to ConnectInfo.
    if let Some(connect_info) = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
    {
        return connect_info.0.ip().to_string();
    }

    "unknown".to_string()
}

/// Returns a 503 SlowDown S3 error response.
fn slow_down_response(resource: &str) -> Response {
    let err = S3Error::new(S3ErrorCode::SlowDown, resource);
    let mut resp = s3_error_response(err);
    resp.headers_mut().insert(
        "Retry-After",
        http::HeaderValue::from_static("1"),
    );
    resp
}
