//! Request ID middleware.
//!
//! Generates a UUID v4 per request and adds the following response headers:
//! - `x-amz-request-id`: the UUID
//! - `x-amz-id-2`: base64-encoded UUID (secondary request identifier)
//! - `Server: Arca`
//!
//! For S3 error XML responses, the middleware also replaces the placeholder
//! `<RequestId>` in the body so it matches the header value.

use axum::body::Body;
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::HeaderName;

use crate::xml::error_response::ErrorRequestId;

pub async fn request_id_middleware(request: axum::extract::Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let id2 = BASE64.encode(request_id.as_bytes());

    let mut response = next.run(request).await;

    // If the response carries an ErrorRequestId extension (from s3_error_response),
    // replace the placeholder request ID in the XML body with the real one.
    if let Some(placeholder) = response.extensions_mut().remove::<ErrorRequestId>() {
        if placeholder.0 != request_id {
            let (mut parts, body) = response.into_parts();
            if let Ok(bytes) = axum::body::to_bytes(body, 64 * 1024).await {
                let xml = String::from_utf8_lossy(&bytes);
                let fixed: String = xml.replace(&placeholder.0, &request_id);
                parts.headers.insert(
                    HeaderName::from_static("x-amz-request-id"),
                    request_id.parse().expect("valid header value"),
                );
                parts.headers.insert(
                    HeaderName::from_static("x-amz-id-2"),
                    id2.parse().expect("valid header value"),
                );
                parts.headers.insert(
                    HeaderName::from_static("server"),
                    "Arca".parse().expect("valid header value"),
                );
                return Response::from_parts(parts, Body::from(fixed));
            }
            tracing::warn!("Failed to read error response body for request ID replacement");
            response = Response::from_parts(parts, Body::empty());
        }
    }

    let headers = response.headers_mut();
    headers.insert(
        HeaderName::from_static("x-amz-request-id"),
        request_id.parse().expect("valid header value"),
    );
    headers.insert(
        HeaderName::from_static("x-amz-id-2"),
        id2.parse().expect("valid header value"),
    );
    headers.insert(
        HeaderName::from_static("server"),
        "Arca".parse().expect("valid header value"),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::routing::get;
    use http::StatusCode;

    async fn dummy_handler() -> &'static str {
        "ok"
    }

    #[tokio::test]
    async fn adds_request_id_headers() {
        let app = Router::new()
            .route("/", get(dummy_handler))
            .layer(axum::middleware::from_fn(request_id_middleware));

        let req = http::Request::builder()
            .uri("/")
            .body(Body::empty())
            .unwrap();

        use tower::ServiceExt as _;
        let response = app.oneshot(req).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("x-amz-request-id"));
        assert!(response.headers().contains_key("x-amz-id-2"));
        assert_eq!(
            response.headers().get("server").unwrap().to_str().unwrap(),
            "Arca"
        );

        // Verify x-amz-request-id is a valid UUID
        let rid = response
            .headers()
            .get("x-amz-request-id")
            .unwrap()
            .to_str()
            .unwrap();
        uuid::Uuid::parse_str(rid).expect("valid UUID");
    }
}
