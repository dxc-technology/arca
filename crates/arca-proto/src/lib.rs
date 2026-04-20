//! arca-proto: S3 HTTP protocol adapter built on Axum.
//!
//! Provides the HTTP routing and handler layer for the Arca S3 server.

pub mod authorize;
pub mod handlers;
pub mod metrics;
pub mod middleware;
pub mod replication;
pub mod router;
pub mod state;
pub mod xml;

pub use router::build_router;
pub use state::AppState;

/// Decode an HTTP header value to a String, handling non-ASCII bytes.
///
/// HTTP header values may contain raw UTF-8 bytes (e.g. botocore sends unicode
/// metadata as UTF-8 on the wire). `HeaderValue::to_str()` rejects non-ASCII,
/// so we fall back to lossy UTF-8 decoding.
pub(crate) fn header_value_to_string(value: &http::HeaderValue) -> String {
    match value.to_str() {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(value.as_bytes()).into_owned(),
    }
}
