//! Trailing-slash normalization with original URI preservation.
//!
//! Provides a tower `Layer` that saves the original URI in request extensions
//! (for auth to verify the signed URI), then strips trailing slashes so Axum
//! routing works for clients like mc that send `/bucket/`.
//!
//! Must be applied **outside** the Axum `Router` (not via `Router::layer()`)
//! because it needs to run *before* Axum routing.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tower::{Layer, Service};

/// The original request URI before path normalization.
/// Used by the auth middleware to verify signatures against
/// the URI the client actually signed.
#[derive(Clone, Debug)]
pub struct OriginalUri(pub http::Uri);

/// Tower layer that saves the original URI and strips trailing slashes.
#[derive(Clone)]
pub struct NormalizeLayer;

impl<S> Layer<S> for NormalizeLayer {
    type Service = NormalizeService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NormalizeService { inner }
    }
}

/// Tower service that saves the original URI in extensions and strips
/// trailing slashes before passing the request to the inner service.
#[derive(Clone)]
pub struct NormalizeService<S> {
    inner: S,
}

impl<S> Service<axum::extract::Request> for NormalizeService<S>
where
    S: Service<axum::extract::Request, Response = axum::response::Response, Error = Infallible>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = axum::response::Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: axum::extract::Request) -> Self::Future {
        // Save original URI for auth verification
        let original = OriginalUri(request.uri().clone());
        request.extensions_mut().insert(original);

        // Strip trailing slash (but not the root "/")
        let path = request.uri().path();
        if path.len() > 1 && path.ends_with('/') {
            let trimmed = &path[..path.len() - 1];
            let new_uri = if let Some(query) = request.uri().query() {
                format!("{trimmed}?{query}")
            } else {
                trimmed.to_string()
            };
            if let Ok(uri) = new_uri.parse() {
                *request.uri_mut() = uri;
            }
        }

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(request).await })
    }
}
