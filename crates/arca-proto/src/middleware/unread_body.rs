//! `Connection: close` on responses sent before the request body was read.
//!
//! When a request is answered before its body has been consumed — any early
//! reject: failed authentication or authorization, a missing bucket, a failed
//! precondition — the unread body bytes are still owed on the HTTP/1.1
//! connection. With `Expect: 100-continue` (which botocore sends on every
//! PutObject) the client, having received a final status instead of
//! `100 Continue`, legitimately never sends them. The server cannot tell
//! whether the bytes that arrive next are the owed body or the next request:
//! hyper may swallow the first bytes of the next request as that body and
//! parse the rest as a mangled request line (`DELETE /b/k` becomes `TE /b/k`,
//! which then fails signature verification). RFC 9110 §10.1.1 asks the server
//! to say whether it will close the connection in this case; AWS S3 closes it.
//!
//! This layer wraps the request body, records whether it was read to the end,
//! and adds `Connection: close` to the response when it was not — whichever
//! middleware or handler produced the response, and whatever its status. It
//! applies to HTTP/1.x only: HTTP/2 frames each stream, so an unread body
//! cannot desync the next request (and `Connection` is not allowed there).
//!
//! Must be applied **outside** the Axum `Router` (like `NormalizeLayer`), so
//! that it also covers responses produced by middleware.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use http::{header, HeaderValue, Version};
use http_body::{Frame, SizeHint};
use tower::{Layer, Service};

/// Tower layer adding `Connection: close` to responses sent while the request
/// body was still unread.
#[derive(Clone)]
pub struct CloseOnUnreadBodyLayer;

impl<S> Layer<S> for CloseOnUnreadBodyLayer {
    type Service = CloseOnUnreadBodyService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        CloseOnUnreadBodyService { inner }
    }
}

/// Tower service behind [`CloseOnUnreadBodyLayer`].
#[derive(Clone)]
pub struct CloseOnUnreadBodyService<S> {
    inner: S,
}

impl<S> Service<axum::extract::Request> for CloseOnUnreadBodyService<S>
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

    fn call(&mut self, request: axum::extract::Request) -> Self::Future {
        let http1 = request.version() < Version::HTTP_2;
        let (parts, body) = request.into_parts();
        let (body, consumed) = TrackedBody::wrap(body);
        let request = axum::extract::Request::from_parts(parts, body);

        let mut inner = self.inner.clone();
        Box::pin(async move {
            let mut response = inner.call(request).await?;
            if http1 && !consumed.load(Ordering::Acquire) {
                response
                    .headers_mut()
                    .insert(header::CONNECTION, HeaderValue::from_static("close"));
            }
            Ok(response)
        })
    }
}

/// A request body that records, in a flag shared with the service, when it
/// has been read to the end. Otherwise transparent: size hint and
/// end-of-stream are forwarded, which body limits and handlers rely on.
struct TrackedBody {
    inner: Body,
    consumed: Arc<AtomicBool>,
}

impl TrackedBody {
    /// Wraps `body`; the returned flag turns true once it is fully read. A
    /// body that is already at its end (no body at all) starts out consumed.
    fn wrap(body: Body) -> (Body, Arc<AtomicBool>) {
        let consumed = Arc::new(AtomicBool::new(http_body::Body::is_end_stream(&body)));
        let tracked = TrackedBody {
            inner: body,
            consumed: consumed.clone(),
        };
        (Body::new(tracked), consumed)
    }
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        // The end is reached either when the stream yields `None` or, for a
        // body of known length, as soon as its last frame is out — a reader
        // may stop there without polling again. A read error is not an end:
        // the rest of the body is still on the wire.
        let ended = matches!(polled, Poll::Ready(None))
            || (matches!(polled, Poll::Ready(Some(Ok(_)))) && this.inner.is_end_stream());
        if ended {
            this.consumed.store(true, Ordering::Release);
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::{Body, Bytes, HttpBody};
    use axum::extract::Request;
    use axum::response::Response;
    use http::{header, StatusCode, Version};
    use tokio_stream::StreamExt;
    use tower::ServiceExt;

    /// How the inner (test) service treats the request body.
    #[derive(Clone, Copy)]
    enum Handling {
        /// Answers without touching the body (an early reject).
        Ignore,
        /// Reads the body to the end, then answers.
        ReadAll,
        /// Reads only the first frame, then answers.
        ReadFirstFrame,
    }

    /// Runs `request` through the layer in front of a service that handles the
    /// body as told and answers `status`. Returns the response and the bytes
    /// the service read.
    async fn run(request: Request, handling: Handling, status: StatusCode) -> (Response, Vec<u8>) {
        let (read_tx, read_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let inner = tower::service_fn(move |req: Request| {
            let read_tx = read_tx.clone();
            async move {
                let body = req.into_body();
                let read = match handling {
                    Handling::Ignore => {
                        drop(body);
                        Vec::new()
                    }
                    Handling::ReadAll => axum::body::to_bytes(body, usize::MAX)
                        .await
                        .unwrap()
                        .to_vec(),
                    Handling::ReadFirstFrame => {
                        let mut stream = body.into_data_stream();
                        stream.next().await.unwrap().unwrap().to_vec()
                    }
                };
                read_tx.send(read).unwrap();
                let mut response = Response::new(Body::empty());
                *response.status_mut() = status;
                Ok::<_, Infallible>(response)
            }
        });
        let response = CloseOnUnreadBodyLayer
            .layer(inner)
            .oneshot(request)
            .await
            .unwrap();
        (response, read_rx.recv().unwrap())
    }

    fn put(body: Body) -> Request {
        Request::builder()
            .method("PUT")
            .uri("/bucket/key")
            .body(body)
            .unwrap()
    }

    fn two_frames() -> Body {
        let chunks: Vec<Result<Bytes, Infallible>> =
            vec![Ok(Bytes::from_static(b"no")), Ok(Bytes::from_static(b"pe"))];
        Body::from_stream(tokio_stream::iter(chunks))
    }

    fn closes(response: &Response) -> bool {
        response
            .headers()
            .get(header::CONNECTION)
            .is_some_and(|v| v == "close")
    }

    #[tokio::test]
    async fn unread_body_closes_the_connection() {
        let (response, _) = run(put(Body::from("nope")), Handling::Ignore, StatusCode::FORBIDDEN).await;
        assert!(closes(&response));
    }

    #[tokio::test]
    async fn unread_body_closes_whatever_the_status() {
        // Not an error-status rule: an unread body is what desyncs the stream.
        let (response, _) = run(put(Body::from("nope")), Handling::Ignore, StatusCode::OK).await;
        assert!(closes(&response));
    }

    #[tokio::test]
    async fn fully_read_body_keeps_the_connection() {
        let (response, read) =
            run(put(Body::from("nope")), Handling::ReadAll, StatusCode::FORBIDDEN).await;
        assert_eq!(read, b"nope");
        assert!(response.headers().get(header::CONNECTION).is_none());
    }

    #[tokio::test]
    async fn fully_read_multi_frame_body_keeps_the_connection() {
        let (response, read) = run(put(two_frames()), Handling::ReadAll, StatusCode::OK).await;
        assert_eq!(read, b"nope");
        assert!(response.headers().get(header::CONNECTION).is_none());
    }

    #[tokio::test]
    async fn partially_read_body_closes_the_connection() {
        let (response, read) =
            run(put(two_frames()), Handling::ReadFirstFrame, StatusCode::BAD_REQUEST).await;
        assert_eq!(read, b"no");
        assert!(closes(&response));
    }

    #[tokio::test]
    async fn reading_the_last_frame_of_a_sized_body_is_enough() {
        // A known-length body is at its end once its last frame is out; a
        // reader that stops there, without polling for `None`, has read it all.
        let (response, read) =
            run(put(Body::from("nope")), Handling::ReadFirstFrame, StatusCode::OK).await;
        assert_eq!(read, b"nope");
        assert!(response.headers().get(header::CONNECTION).is_none());
    }

    #[tokio::test]
    async fn request_without_body_keeps_the_connection() {
        let request = Request::builder().uri("/bucket/key").body(Body::empty()).unwrap();
        let (response, _) = run(request, Handling::Ignore, StatusCode::NOT_FOUND).await;
        assert!(response.headers().get(header::CONNECTION).is_none());
    }

    #[tokio::test]
    async fn http2_is_left_alone() {
        // HTTP/2 frames each stream (no desync possible) and forbids the
        // `Connection` header.
        let mut request = put(Body::from("nope"));
        *request.version_mut() = Version::HTTP_2;
        let (response, _) = run(request, Handling::Ignore, StatusCode::FORBIDDEN).await;
        assert!(response.headers().get(header::CONNECTION).is_none());
    }

    #[tokio::test]
    async fn http10_is_covered() {
        let mut request = put(Body::from("nope"));
        *request.version_mut() = Version::HTTP_10;
        let (response, _) = run(request, Handling::Ignore, StatusCode::FORBIDDEN).await;
        assert!(closes(&response));
    }

    #[tokio::test]
    async fn body_reaches_the_handler_unchanged() {
        // The wrapper must be transparent: size hint and end-of-stream are
        // what body limits and streaming handlers rely on.
        let inner = tower::service_fn(|req: Request| async move {
            let body = req.into_body();
            assert_eq!(body.size_hint().exact(), Some(4));
            assert!(!body.is_end_stream());
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            assert_eq!(&bytes[..], b"nope");
            Ok::<_, Infallible>(Response::new(Body::empty()))
        });
        let response = CloseOnUnreadBodyLayer
            .layer(inner)
            .oneshot(put(Body::from("nope")))
            .await
            .unwrap();
        assert!(response.headers().get(header::CONNECTION).is_none());
    }
}
