//! Request-path normalization with original URI preservation.
//!
//! Provides a tower `Layer` that saves the original URI in request extensions
//! (for auth to verify the signed URI), rewrites a virtual-hosted-style
//! request (`bucket.{domain}/key`, when a server domain is configured) to
//! path style, then strips trailing slashes so Axum routing works for clients
//! like mc that send `/bucket/`. The rewrite comes before the slash stripping
//! because `/dir/` is a key, not a bucket path, once the bucket is in the host.
//!
//! Must be applied **outside** the Axum `Router` (not via `Router::layer()`)
//! because it needs to run *before* Axum routing.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use super::virtual_host;

/// The original request URI before path normalization.
/// Used by the auth middleware to verify signatures against
/// the URI the client actually signed.
#[derive(Clone, Debug)]
pub struct OriginalUri(pub http::Uri);

/// Tower layer that saves the original URI, rewrites virtual-hosted-style
/// requests to path style and strips trailing slashes.
#[derive(Clone)]
pub struct NormalizeLayer {
    /// `[server] domain`: `bucket.{domain}` hosts address a bucket.
    domain: Option<Arc<str>>,
}

impl NormalizeLayer {
    pub fn new(domain: Option<String>) -> Self {
        Self {
            domain: domain.map(Arc::from),
        }
    }
}

impl<S> Layer<S> for NormalizeLayer {
    type Service = NormalizeService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        NormalizeService {
            inner,
            domain: self.domain.clone(),
        }
    }
}

/// Tower service that saves the original URI in extensions and strips
/// trailing slashes before passing the request to the inner service.
#[derive(Clone)]
pub struct NormalizeService<S> {
    inner: S,
    domain: Option<Arc<str>>,
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

        if let Some(domain) = self.domain.as_deref() {
            rewrite_virtual_host(&mut request, domain);
        }

        // Strip trailing slash only for bucket-level paths (e.g. `/bucket/`)
        // but NOT for object-key paths (e.g. `/bucket/folder/`), because a
        // trailing slash is significant in S3 object keys (directory markers).
        let path = request.uri().path();
        if path.len() > 1 && path.ends_with('/') {
            // Count segments: "/bucket/" has 1, "/bucket/key/" has 2+
            let segment_count = path[1..path.len() - 1].matches('/').count() + 1;
            if segment_count <= 1 {
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
        }

        let mut inner = self.inner.clone();
        Box::pin(async move { inner.call(request).await })
    }
}

/// Rewrites a virtual-hosted-style request (`Host: {bucket}.{domain}`) to the
/// path-style URI the routes expect. The host comes from the `Host` header or,
/// for HTTP/2 where hyper does not synthesize one, the URI authority.
fn rewrite_virtual_host(request: &mut axum::extract::Request, domain: &str) {
    let host = request
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()));
    let Some(bucket) = host.and_then(|h| virtual_host::bucket_for_host(h, domain)) else {
        return;
    };
    let path = virtual_host::path_style_path(bucket, request.uri().path());
    let new_uri = match request.uri().query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    if let Ok(uri) = new_uri.parse() {
        *request.uri_mut() = uri;
    }
}

#[cfg(test)]
mod tests {
    /// Helper: determine what the normalize logic does to a given path.
    /// Returns the normalized path (or the original if unchanged).
    fn normalize_path(path: &str) -> String {
        if path.len() > 1 && path.ends_with('/') {
            let segment_count = path[1..path.len() - 1].matches('/').count() + 1;
            if segment_count <= 1 {
                return path[..path.len() - 1].to_string();
            }
        }
        path.to_string()
    }

    #[test]
    fn root_unchanged() {
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn bucket_path_no_trailing_slash() {
        assert_eq!(normalize_path("/mybucket"), "/mybucket");
    }

    #[test]
    fn bucket_path_trailing_slash_stripped() {
        assert_eq!(normalize_path("/mybucket/"), "/mybucket");
    }

    #[test]
    fn object_key_no_trailing_slash() {
        assert_eq!(normalize_path("/bucket/key"), "/bucket/key");
    }

    #[test]
    fn object_key_trailing_slash_preserved() {
        // Directory marker: trailing slash must NOT be stripped
        assert_eq!(normalize_path("/bucket/folder/"), "/bucket/folder/");
    }

    #[test]
    fn nested_object_key_trailing_slash_preserved() {
        assert_eq!(normalize_path("/bucket/a/b/"), "/bucket/a/b/");
    }

    #[test]
    fn deeply_nested_trailing_slash_preserved() {
        assert_eq!(normalize_path("/bucket/a/b/c/d/"), "/bucket/a/b/c/d/");
    }

    #[test]
    fn admin_path_trailing_slash_stripped() {
        // /admin/ is a single-segment path, should be stripped
        assert_eq!(normalize_path("/admin/"), "/admin");
    }

    #[test]
    fn admin_sub_path_trailing_slash_preserved() {
        // /admin/health/ has 2 segments, should be preserved
        assert_eq!(normalize_path("/admin/health/"), "/admin/health/");
    }

    use super::{NormalizeLayer, OriginalUri};
    use axum::body::Body;
    use axum::extract::Request;
    use axum::response::Response;
    use std::convert::Infallible;
    use tower::{service_fn, Layer, ServiceExt};

    /// Runs `request` through the layer and returns the URI the inner service
    /// saw and the URI recorded for auth.
    async fn through(domain: Option<&str>, request: Request) -> (String, String) {
        let inner = service_fn(|req: Request| async move {
            let original = req.extensions().get::<OriginalUri>().unwrap().0.to_string();
            let mut response = Response::new(Body::empty());
            response
                .headers_mut()
                .insert("x-seen", req.uri().to_string().parse().unwrap());
            response
                .headers_mut()
                .insert("x-original", original.parse().unwrap());
            Ok::<_, Infallible>(response)
        });
        let response = NormalizeLayer::new(domain.map(str::to_string))
            .layer(inner)
            .oneshot(request)
            .await
            .unwrap();
        let header = |name: &str| response.headers()[name].to_str().unwrap().to_string();
        (header("x-seen"), header("x-original"))
    }

    fn get(uri: &str, host: &str) -> Request {
        Request::builder()
            .uri(uri)
            .header("host", host)
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn a_virtual_hosted_key_is_routed_in_the_bucket() {
        let (seen, original) = through(Some("s3.test"), get("/dir/key", "b.s3.test:9000")).await;
        assert_eq!(seen, "/b/dir/key");
        assert_eq!(original, "/dir/key", "auth verifies what the client signed");
    }

    #[tokio::test]
    async fn the_virtual_hosted_bucket_root_is_the_bucket_not_the_service() {
        let (seen, original) = through(Some("s3.test"), get("/?list-type=2&prefix=p", "b.s3.test")).await;
        assert_eq!(seen, "/b?list-type=2&prefix=p");
        assert_eq!(original, "/?list-type=2&prefix=p");
    }

    #[tokio::test]
    async fn a_virtual_hosted_directory_marker_keeps_its_trailing_slash() {
        // `/dir/` is a key here, not a bucket path to be stripped.
        let (seen, _) = through(Some("s3.test"), get("/dir/", "b.s3.test")).await;
        assert_eq!(seen, "/b/dir/");
    }

    #[tokio::test]
    async fn a_path_style_request_on_the_domain_is_unchanged() {
        let (seen, _) = through(Some("s3.test"), get("/b/key", "s3.test")).await;
        assert_eq!(seen, "/b/key");
        let (seen, _) = through(Some("s3.test"), get("/b/", "s3.test")).await;
        assert_eq!(seen, "/b");
    }

    #[tokio::test]
    async fn without_a_domain_the_host_is_never_a_bucket() {
        let (seen, _) = through(None, get("/key", "b.s3.test")).await;
        assert_eq!(seen, "/key");
    }

    #[tokio::test]
    async fn http2_authority_addresses_the_bucket_when_there_is_no_host_header() {
        let request = Request::builder()
            .uri("http://b.s3.test/key")
            .body(Body::empty())
            .unwrap();
        let (seen, _) = through(Some("s3.test"), request).await;
        assert_eq!(seen, "/b/key");
    }
}
