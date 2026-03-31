//! Shared body stream utilities.
//!
//! Provides `body_to_byte_stream()` which converts an Axum body into a `ByteStream`,
//! transparently decoding AWS chunked transfer encoding when present.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use http::HeaderMap;
use tokio_stream::Stream;

use arca_core::store::ByteStream;

/// Converts an Axum body into a `ByteStream`.
///
/// If the `x-amz-content-sha256` header starts with `STREAMING-`, the body
/// uses AWS chunked transfer encoding and will be transparently decoded.
///
/// When `max_body_size` is `Some(n)` with `n > 0`, the stream enforces a byte
/// limit and returns an `EntityTooLarge` I/O error if exceeded.
pub fn body_to_byte_stream(body: Body, headers: &HeaderMap, max_body_size: Option<u64>) -> ByteStream {
    use tokio_stream::StreamExt;

    let is_chunked = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("STREAMING-"));

    let stream = body.into_data_stream();
    let mapped: ByteStream = Box::pin(stream.map(|result| {
        result.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }));

    let decoded = if is_chunked {
        Box::pin(AwsChunkedDecoder::new(mapped)) as ByteStream
    } else {
        mapped
    };

    match max_body_size {
        Some(limit) if limit > 0 => Box::pin(LimitedByteStream::new(decoded, limit)),
        _ => decoded,
    }
}

/// Wraps a `ByteStream` and enforces a maximum byte count.
///
/// Once the cumulative bytes exceed `limit`, the stream yields an I/O error
/// with kind `Other` and message "EntityTooLarge" (used by handlers to return
/// the appropriate S3 error response).
pub(crate) struct LimitedByteStream {
    inner: ByteStream,
    limit: u64,
    bytes_read: u64,
}

impl LimitedByteStream {
    pub fn new(inner: ByteStream, limit: u64) -> Self {
        Self {
            inner,
            limit,
            bytes_read: 0,
        }
    }
}

impl Stream for LimitedByteStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = unsafe { self.get_unchecked_mut() };

        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.bytes_read += chunk.len() as u64;
                if this.bytes_read > this.limit {
                    Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::Other,
                        "EntityTooLarge",
                    ))))
                } else {
                    Poll::Ready(Some(Ok(chunk)))
                }
            }
            other => other,
        }
    }
}

/// Decodes an AWS chunked transfer encoding stream.
///
/// AWS chunked format:
/// ```text
/// <hex-size>;chunk-signature=<sig>\r\n
/// <data bytes>\r\n
/// 0;chunk-signature=<sig>\r\n
/// \r\n
/// ```
///
/// Each chunk has a header line with the hex size (optionally followed by
/// `;chunk-signature=...`), then `\r\n`, then the data bytes, then `\r\n`.
/// A chunk with size 0 terminates the stream.
struct AwsChunkedDecoder {
    inner: ByteStream,
    buf: BytesMut,
    state: DecoderState,
}

#[derive(Debug)]
enum DecoderState {
    /// Waiting for the chunk header line (`<hex>[;chunk-signature=...]\r\n`).
    ReadingHeader,
    /// Reading `remaining` data bytes from the current chunk.
    ReadingData { remaining: usize },
    /// Expecting `\r\n` after chunk data.
    ReadingTrailer { to_skip: usize },
    /// Terminal chunk received, stream is done.
    Done,
}

impl AwsChunkedDecoder {
    fn new(inner: ByteStream) -> Self {
        Self {
            inner,
            buf: BytesMut::new(),
            state: DecoderState::ReadingHeader,
        }
    }
}

impl Stream for AwsChunkedDecoder {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = unsafe { self.get_unchecked_mut() };

        loop {
            match this.state {
                DecoderState::Done => return Poll::Ready(None),

                DecoderState::ReadingHeader => {
                    // Look for \r\n in buffer.
                    if let Some(pos) = find_crlf(&this.buf) {
                        let header_line = &this.buf[..pos];
                        let chunk_size = parse_chunk_header(header_line)?;
                        // Consume header + \r\n.
                        let _ = this.buf.split_to(pos + 2);

                        if chunk_size == 0 {
                            this.state = DecoderState::Done;
                            return Poll::Ready(None);
                        }
                        this.state = DecoderState::ReadingData {
                            remaining: chunk_size,
                        };
                        continue;
                    }
                    // Need more data.
                    match Pin::new(&mut this.inner).poll_next(cx) {
                        Poll::Ready(Some(Ok(bytes))) => {
                            this.buf.extend_from_slice(&bytes);
                            continue;
                        }
                        Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                        Poll::Ready(None) => {
                            // Stream ended mid-header: if buffer empty, we're done.
                            if this.buf.is_empty() {
                                this.state = DecoderState::Done;
                                return Poll::Ready(None);
                            }
                            return Poll::Ready(Some(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "Incomplete AWS chunked header",
                            ))));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }

                DecoderState::ReadingData { remaining } => {
                    if this.buf.is_empty() {
                        // Need more data.
                        match Pin::new(&mut this.inner).poll_next(cx) {
                            Poll::Ready(Some(Ok(bytes))) => {
                                this.buf.extend_from_slice(&bytes);
                                continue;
                            }
                            Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                            Poll::Ready(None) => {
                                return Poll::Ready(Some(Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "Incomplete AWS chunked data",
                                ))));
                            }
                            Poll::Pending => return Poll::Pending,
                        }
                    }

                    let to_take = remaining.min(this.buf.len());
                    let data = this.buf.split_to(to_take).freeze();
                    let new_remaining = remaining - to_take;

                    if new_remaining == 0 {
                        this.state = DecoderState::ReadingTrailer { to_skip: 2 };
                    } else {
                        this.state = DecoderState::ReadingData {
                            remaining: new_remaining,
                        };
                    }

                    return Poll::Ready(Some(Ok(data)));
                }

                DecoderState::ReadingTrailer { to_skip } => {
                    if this.buf.len() >= to_skip {
                        let _ = this.buf.split_to(to_skip);
                        this.state = DecoderState::ReadingHeader;
                        continue;
                    }
                    // Need more data.
                    match Pin::new(&mut this.inner).poll_next(cx) {
                        Poll::Ready(Some(Ok(bytes))) => {
                            this.buf.extend_from_slice(&bytes);
                            continue;
                        }
                        Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                        Poll::Ready(None) => {
                            // Stream ended while we still have trailer to skip.
                            // Be lenient: the final chunk's \r\n might be missing.
                            this.state = DecoderState::Done;
                            return Poll::Ready(None);
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
            }
        }
    }
}

/// Finds the position of the first `\r\n` in the buffer.
fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Parses a chunk header line: `<hex-size>[;chunk-signature=...]`.
fn parse_chunk_header(line: &[u8]) -> Result<usize, io::Error> {
    let line = std::str::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Non-UTF-8 chunk header"))?;

    // Size is before the first semicolon (if any).
    let hex_part = line.split(';').next().unwrap_or(line).trim();

    usize::from_str_radix(hex_part, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid chunk size hex"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    /// Helper: build a ByteStream from raw bytes.
    fn stream_from_bytes(data: &[u8]) -> ByteStream {
        let bytes = Bytes::copy_from_slice(data);
        Box::pin(tokio_stream::once(Ok(bytes)))
    }

    /// Helper: collect a ByteStream into a Vec<u8>.
    async fn collect_stream(stream: ByteStream) -> Result<Vec<u8>, io::Error> {
        let mut result = Vec::new();
        tokio::pin!(stream);
        while let Some(chunk) = stream.next().await {
            result.extend_from_slice(&chunk?);
        }
        Ok(result)
    }

    #[tokio::test]
    async fn decode_single_chunk() {
        // "hello" = 5 bytes
        let raw = b"5;chunk-signature=abc123\r\nhello\r\n0;chunk-signature=def456\r\n\r\n";
        let decoded = collect_stream(Box::pin(AwsChunkedDecoder::new(stream_from_bytes(raw))))
            .await
            .unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[tokio::test]
    async fn decode_multi_chunk() {
        let raw =
            b"5;chunk-signature=aaa\r\nhello\r\n6;chunk-signature=bbb\r\n world\r\n0;chunk-signature=ccc\r\n\r\n";
        let decoded = collect_stream(Box::pin(AwsChunkedDecoder::new(stream_from_bytes(raw))))
            .await
            .unwrap();
        assert_eq!(decoded, b"hello world");
    }

    #[tokio::test]
    async fn decode_empty_body() {
        let raw = b"0;chunk-signature=abc\r\n\r\n";
        let decoded = collect_stream(Box::pin(AwsChunkedDecoder::new(stream_from_bytes(raw))))
            .await
            .unwrap();
        assert!(decoded.is_empty());
    }

    #[tokio::test]
    async fn decode_without_chunk_signature() {
        // Plain hex size without signature extension.
        let raw = b"3\r\nfoo\r\n0\r\n\r\n";
        let decoded = collect_stream(Box::pin(AwsChunkedDecoder::new(stream_from_bytes(raw))))
            .await
            .unwrap();
        assert_eq!(decoded, b"foo");
    }

    #[tokio::test]
    async fn decode_split_across_stream_chunks() {
        // Data arrives in multiple stream chunks.
        let part1 = Bytes::from_static(b"5;chunk-signature=a\r\nhel");
        let part2 = Bytes::from_static(b"lo\r\n0;chunk-signature=b\r\n\r\n");

        let inner: ByteStream = Box::pin(tokio_stream::iter(vec![Ok(part1), Ok(part2)]));
        let decoded = collect_stream(Box::pin(AwsChunkedDecoder::new(inner)))
            .await
            .unwrap();
        assert_eq!(decoded, b"hello");
    }

    #[tokio::test]
    async fn body_to_byte_stream_passthrough_for_normal_body() {
        let body = Body::from("hello world");
        let headers = HeaderMap::new();
        let stream = body_to_byte_stream(body, &headers, None);
        let result = collect_stream(stream).await.unwrap();
        assert_eq!(result, b"hello world");
    }

    #[tokio::test]
    async fn body_to_byte_stream_decodes_aws_chunked() {
        let raw = b"5;chunk-signature=abc\r\nhello\r\n0;chunk-signature=def\r\n\r\n";
        let body = Body::from(raw.to_vec());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-content-sha256",
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD".parse().unwrap(),
        );
        let stream = body_to_byte_stream(body, &headers, None);
        let result = collect_stream(stream).await.unwrap();
        assert_eq!(result, b"hello");
    }

    #[tokio::test]
    async fn limited_stream_under_limit_passes() {
        let data = b"hello";
        let inner: ByteStream = Box::pin(tokio_stream::once(Ok(Bytes::from_static(data))));
        let stream = LimitedByteStream::new(inner, 100);
        let result = collect_stream(Box::pin(stream)).await.unwrap();
        assert_eq!(result, b"hello");
    }

    #[tokio::test]
    async fn limited_stream_over_limit_returns_error() {
        let data = b"hello world, this is a longer payload";
        let inner: ByteStream = Box::pin(tokio_stream::once(Ok(Bytes::from_static(data))));
        let stream = LimitedByteStream::new(inner, 10);
        let result = collect_stream(Box::pin(stream)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("EntityTooLarge"));
    }

    #[tokio::test]
    async fn limited_stream_exact_limit_passes() {
        let data = b"12345";
        let inner: ByteStream = Box::pin(tokio_stream::once(Ok(Bytes::from_static(data))));
        let stream = LimitedByteStream::new(inner, 5);
        let result = collect_stream(Box::pin(stream)).await.unwrap();
        assert_eq!(result, b"12345");
    }

    #[tokio::test]
    async fn limited_stream_zero_limit_is_unlimited() {
        let data = b"hello world";
        let body = Body::from(data.to_vec());
        let headers = HeaderMap::new();
        // max_body_size = Some(0) should mean unlimited
        let stream = body_to_byte_stream(body, &headers, Some(0));
        let result = collect_stream(stream).await.unwrap();
        assert_eq!(result, b"hello world");
    }
}
