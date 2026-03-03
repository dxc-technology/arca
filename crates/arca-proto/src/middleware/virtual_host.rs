//! Virtual-hosted-style request rewrite middleware.
//!
//! Converts `bucket.example.com/key` → `/bucket/key` for routing,
//! when a server domain is configured.

use axum::extract::State;
use axum::response::Response;

use crate::state::AppState;

/// Axum middleware that rewrites virtual-hosted-style bucket requests.
///
/// If the `Host` header matches `{bucket}.{domain}` (where `domain` is
/// configured in AppState), the URI path is rewritten to prepend `/{bucket}`.
///
/// This middleware runs AFTER auth, so auth verifies the original URI
/// (what the client signed).
pub async fn virtual_host_middleware(
    State(state): State<AppState>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(ref domain) = state.domain {
        if let Some(host_header) = request.headers().get(http::header::HOST) {
            if let Ok(host_str) = host_header.to_str() {
                // Strip port if present
                let host = host_str.split(':').next().unwrap_or(host_str);

                // Check if host ends with .{domain}
                let suffix = format!(".{domain}");
                if let Some(bucket) = host.strip_suffix(&suffix) {
                    if !bucket.is_empty() && !bucket.contains('.') {
                        // Rewrite URI: prepend /{bucket} to path
                        let original_path = request.uri().path();
                        let new_path = format!("/{bucket}{original_path}");

                        // Reconstruct URI with new path + original query
                        let new_uri = if let Some(query) = request.uri().query() {
                            format!("{new_path}?{query}")
                        } else {
                            new_path
                        };

                        if let Ok(uri) = new_uri.parse() {
                            *request.uri_mut() = uri;
                        }
                    }
                }
            }
        }
    }

    next.run(request).await
}
