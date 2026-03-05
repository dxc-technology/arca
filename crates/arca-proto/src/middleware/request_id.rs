//! Request ID middleware.
//!
//! Generates a UUID v4 per request and adds the following response headers:
//! - `x-amz-request-id`: the UUID
//! - `x-amz-id-2`: base64-encoded UUID (secondary request identifier)
//! - `Server: Arca`

use axum::body::Body;
use axum::middleware::Next;
use axum::response::Response;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use http::HeaderName;

pub async fn request_id_middleware(request: axum::extract::Request, next: Next) -> Response {
    let request_id = uuid::Uuid::new_v4().to_string();
    let id2 = BASE64.encode(request_id.as_bytes());

    let mut response = next.run(request).await;

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
