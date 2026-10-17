//! Shared body stream utilities.
//!
//! Provides `body_to_byte_stream()` which converts an Axum body into a `ByteStream`,
//! transparently decoding AWS chunked transfer encoding when present, and
//! `verified_body_stream()`, which additionally verifies the body against the
//! digests the request declares (see `handlers::integrity`).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use http::HeaderMap;
use tokio_stream::Stream;

use arca_auth::{canonical_trailer, ChunkVerifier};
use arca_core::store::ByteStream;
use arca_core::{S3Error, S3ErrorCode};
use sha2::{Digest, Sha256};

use super::integrity::{BodyIntegrity, ChecksumAlgorithm, IntegrityCheck, Trailers};

/// Message of the I/O error a stream yields when a chunk or trailer signature
/// does not verify (or the signed body is incomplete). Handlers turn it into
/// `SignatureDoesNotMatch`, the way they do `EntityTooLarge`.
pub(crate) const SIGNATURE_MISMATCH: &str = "SignatureDoesNotMatch";

fn signature_mismatch() -> io::Error {
    io::Error::new(io::ErrorKind::Other, SIGNATURE_MISMATCH)
}

/// The S3 error a failed body stream stands for (`EntityTooLarge` from the size
/// limit, `SignatureDoesNotMatch` from a chunk signature), read from the
/// message of the error the blob store hands back; `None` for anything else.
pub(crate) fn stream_error_code(message: &str) -> Option<S3ErrorCode> {
    if message.contains("EntityTooLarge") {
        Some(S3ErrorCode::EntityTooLarge)
    } else if message.contains(SIGNATURE_MISMATCH) {
        Some(S3ErrorCode::SignatureDoesNotMatch)
    } else {
        None
    }
}

/// Converts an Axum body into a `ByteStream`.
///
/// If the `x-amz-content-sha256` header starts with `STREAMING-`, the body
/// uses AWS chunked transfer encoding and will be transparently decoded.
/// Chunk signatures are NOT verified here: this is for bodies that are not
/// signed by a client (inter-node transfers). Client uploads go through
/// [`verified_body_stream`].
///
/// When `max_body_size` is `Some(n)` with `n > 0`, the stream enforces a byte
/// limit and returns an `EntityTooLarge` I/O error if exceeded.
pub fn body_to_byte_stream(body: Body, headers: &HeaderMap, max_body_size: Option<u64>) -> ByteStream {
    decoded_body_stream(body, headers, max_body_size, None).0
}

/// Like [`body_to_byte_stream`], and verifies the body against the request's
/// `Content-MD5`, `x-amz-checksum-*` (header or trailer) and hex
/// `x-amz-content-sha256`.
///
/// A signed aws-chunked body (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`, with or
/// without `-TRAILER`) is authenticated chunk by chunk against `chunk_verifier`
/// (the request's signature chain, see `arca_auth::ChunkVerifier`): a chunk or
/// trailer that does not verify, a missing signature and a body cut before its
/// final chunk all end the stream with [`SIGNATURE_MISMATCH`]. Such a body
/// without a verifier (presigned URLs have no seed signature) is refused here.
///
/// A malformed declaration is refused here, before any byte is read. The
/// returned [`IntegrityCheck`] must be verified once the blob store has
/// consumed the stream, before the object is committed.
pub fn verified_body_stream(
    body: Body,
    headers: &HeaderMap,
    max_body_size: Option<u64>,
    resource: &str,
    chunk_verifier: Option<ChunkVerifier>,
) -> Result<(ByteStream, IntegrityCheck), S3Error> {
    verified_body_stream_requiring(body, headers, max_body_size, resource, chunk_verifier, None)
}

/// [`verified_body_stream`] for the part of an upload created with a checksum
/// algorithm (`required`): the part must carry that checksum, which is computed
/// when the request declares none and refused when it declares another
/// algorithm (see `BodyIntegrity::require_checksum`).
pub fn verified_body_stream_requiring(
    body: Body,
    headers: &HeaderMap,
    max_body_size: Option<u64>,
    resource: &str,
    chunk_verifier: Option<ChunkVerifier>,
    required: Option<ChecksumAlgorithm>,
) -> Result<(ByteStream, IntegrityCheck), S3Error> {
    let mut integrity = BodyIntegrity::from_headers(headers, resource)?;
    if let Some(algorithm) = required {
        integrity.require_checksum(algorithm, resource)?;
    }
    let signing = match (signed_streaming_mode(headers), chunk_verifier) {
        (Some(with_trailer), Some(verifier)) => Some((verifier, with_trailer)),
        (Some(_), None) => {
            return Err(S3Error::new(S3ErrorCode::SignatureDoesNotMatch, resource));
        }
        (None, _) => None,
    };
    let (stream, trailers) = decoded_body_stream(body, headers, max_body_size, signing);
    Ok(integrity.wrap(stream, trailers))
}

/// `Some(with_trailer)` when `x-amz-content-sha256` declares a body whose
/// chunks are signed (`...-PAYLOAD`, or `...-PAYLOAD-TRAILER` for a signed
/// trailer too), `None` for anything else.
fn signed_streaming_mode(headers: &HeaderMap) -> Option<bool> {
    match headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())?
    {
        "STREAMING-AWS4-HMAC-SHA256-PAYLOAD" => Some(false),
        "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER" => Some(true),
        _ => None,
    }
}

/// Builds the decoded (and size-limited) body stream; for an aws-chunked body
/// also returns the sink its trailing headers are collected into. `signing`
/// (the verifier and whether the trailer is signed too) turns on signature
/// verification.
fn decoded_body_stream(
    body: Body,
    headers: &HeaderMap,
    max_body_size: Option<u64>,
    signing: Option<(ChunkVerifier, bool)>,
) -> (ByteStream, Option<Trailers>) {
    use tokio_stream::StreamExt;

    let is_chunked = headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("STREAMING-"));

    let stream = body.into_data_stream();
    let mapped: ByteStream = Box::pin(stream.map(|result| {
        result.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }));

    let (decoded, trailers) = if is_chunked {
        let trailers: Trailers = Arc::new(Mutex::new(Vec::new()));
        let mut decoder = AwsChunkedDecoder::new(mapped).with_trailers(trailers.clone());
        if let Some((verifier, with_trailer)) = signing {
            decoder = decoder.with_signature_verification(verifier, with_trailer);
        }
        (Box::pin(decoder) as ByteStream, Some(trailers))
    } else {
        (mapped, None)
    };

    let limited = match max_body_size {
        Some(limit) if limit > 0 => Box::pin(LimitedByteStream::new(decoded, limit)) as ByteStream,
        _ => decoded,
    };
    (limited, trailers)
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
/// [<trailer-name>:<value>\r\n ...]
/// \r\n
/// ```
///
/// Each chunk has a header line with the hex size (optionally followed by
/// `;chunk-signature=...`), then `\r\n`, then the data bytes, then `\r\n`.
/// A chunk with size 0 terminates the data; it is followed by the trailing
/// headers (`STREAMING-*-TRAILER` payloads, e.g. `x-amz-checksum-crc32:...`)
/// and an empty line. Trailers are collected into the sink set with
/// [`Self::with_trailers`].
///
/// By default the `chunk-signature=` extensions and `x-amz-trailer-signature`
/// are stripped, not checked (unsigned bodies, inter-node transfers).
/// [`Self::with_signature_verification`] makes the decoder authenticate every
/// chunk and the trailer against the request's signature chain, and treat a
/// body that does not end with its signed final chunk as an error.
struct AwsChunkedDecoder {
    inner: ByteStream,
    buf: BytesMut,
    state: DecoderState,
    trailers: Option<Trailers>,
    signed: Option<SignedChunks>,
}

/// State of the signature chain of a signed body.
struct SignedChunks {
    verifier: ChunkVerifier,
    /// `-PAYLOAD-TRAILER`: a signed trailer must follow the final chunk.
    with_trailer: bool,
    /// Hash of the data of the chunk being read.
    hasher: Sha256,
    /// Signature announced by the header of the chunk being read.
    chunk_signature: String,
    /// Trailer headers, in order, without the signature line.
    trailer_lines: Vec<(String, String)>,
    trailer_signature: Option<String>,
}

impl SignedChunks {
    /// Verifies the chunk whose data was just hashed.
    fn finish_chunk(&mut self) -> Result<(), io::Error> {
        let hash = hex::encode(std::mem::replace(&mut self.hasher, Sha256::new()).finalize());
        self.verifier
            .verify_chunk(&hash, &self.chunk_signature)
            .map_err(|_| signature_mismatch())
    }

    /// Takes one trailer line (`name:value`); a line that is not one cannot be
    /// authenticated.
    fn add_trailer_line(&mut self, line: &[u8]) -> Result<(), io::Error> {
        let line = std::str::from_utf8(line).map_err(|_| signature_mismatch())?;
        let (name, value) = line.split_once(':').ok_or_else(signature_mismatch)?;
        if name.trim().eq_ignore_ascii_case("x-amz-trailer-signature") {
            self.trailer_signature = Some(value.trim().to_string());
        } else {
            self.trailer_lines
                .push((name.trim().to_string(), value.trim().to_string()));
        }
        Ok(())
    }

    /// Verifies the trailer once the blank line that ends it has been read.
    fn finish_trailer(&mut self) -> Result<(), io::Error> {
        if !self.with_trailer {
            // `-PAYLOAD` has no signed trailer: anything there is unauthenticated.
            return if self.trailer_lines.is_empty() && self.trailer_signature.is_none() {
                Ok(())
            } else {
                Err(signature_mismatch())
            };
        }
        let signature = self.trailer_signature.take().ok_or_else(signature_mismatch)?;
        let canonical = canonical_trailer(
            self.trailer_lines
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str())),
        );
        self.verifier
            .verify_trailer(&canonical, &signature)
            .map_err(|_| signature_mismatch())
    }
}

#[derive(Debug)]
enum DecoderState {
    /// Waiting for the chunk header line (`<hex>[;chunk-signature=...]\r\n`).
    ReadingHeader,
    /// Reading `remaining` data bytes from the current chunk.
    ReadingData { remaining: usize },
    /// Expecting `\r\n` after chunk data.
    ReadingTrailer { to_skip: usize },
    /// Terminal chunk received; reading trailing header lines up to the
    /// empty line.
    ReadingTrailingHeaders,
    /// Terminal chunk and trailers received, stream is done.
    Done,
}

impl AwsChunkedDecoder {
    fn new(inner: ByteStream) -> Self {
        Self {
            inner,
            buf: BytesMut::new(),
            state: DecoderState::ReadingHeader,
            trailers: None,
            signed: None,
        }
    }

    /// Authenticates the body against `verifier` (see the type docs).
    /// `with_trailer` is true for `-PAYLOAD-TRAILER` bodies.
    fn with_signature_verification(mut self, verifier: ChunkVerifier, with_trailer: bool) -> Self {
        self.signed = Some(SignedChunks {
            verifier,
            with_trailer,
            hasher: Sha256::new(),
            chunk_signature: String::new(),
            trailer_lines: Vec::new(),
            trailer_signature: None,
        });
        self
    }

    /// Collects the trailing headers into `sink` (names lowercased).
    fn with_trailers(mut self, sink: Trailers) -> Self {
        self.trailers = Some(sink);
        self
    }

    /// Records one `name:value` trailer line; other lines are ignored.
    fn record_trailer(&self, line: &[u8]) {
        let (Some(sink), Ok(line)) = (self.trailers.as_ref(), std::str::from_utf8(line)) else {
            return;
        };
        if let Some((name, value)) = line.split_once(':') {
            sink.lock()
                .expect("trailers lock")
                .push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
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
                        let (chunk_size, signature) = parse_chunk_header(header_line)?;
                        // Consume header + \r\n.
                        let _ = this.buf.split_to(pos + 2);

                        if let Some(signed) = this.signed.as_mut() {
                            // A signed body whose chunk has no signature is a
                            // body someone stripped them from.
                            let Some(signature) = signature else {
                                this.state = DecoderState::Done;
                                return Poll::Ready(Some(Err(signature_mismatch())));
                            };
                            signed.chunk_signature = signature;
                        }

                        if chunk_size == 0 {
                            // The final chunk: signed over empty data.
                            if let Some(signed) = this.signed.as_mut() {
                                if let Err(e) = signed.finish_chunk() {
                                    this.state = DecoderState::Done;
                                    return Poll::Ready(Some(Err(e)));
                                }
                            }
                            this.state = DecoderState::ReadingTrailingHeaders;
                            continue;
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
                            // A signed body must end with its signed final
                            // chunk: without it nothing says the data was not
                            // cut after a valid chunk (or inside one).
                            if this.signed.is_some() {
                                this.state = DecoderState::Done;
                                return Poll::Ready(Some(Err(signature_mismatch())));
                            }
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
                                if this.signed.is_some() {
                                    this.state = DecoderState::Done;
                                    return Poll::Ready(Some(Err(signature_mismatch())));
                                }
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

                    if let Some(signed) = this.signed.as_mut() {
                        signed.hasher.update(&data);
                        if new_remaining == 0 {
                            // The chunk is complete: its signature must hold
                            // before any later chunk is accepted. The error
                            // replaces the last bytes; the upload is aborted
                            // as a whole, nothing of it is committed.
                            if let Err(e) = signed.finish_chunk() {
                                this.state = DecoderState::Done;
                                return Poll::Ready(Some(Err(e)));
                            }
                        }
                    }

                    if new_remaining == 0 {
                        this.state = DecoderState::ReadingTrailer { to_skip: 2 };
                    } else {
                        this.state = DecoderState::ReadingData {
                            remaining: new_remaining,
                        };
                    }

                    return Poll::Ready(Some(Ok(data)));
                }

                DecoderState::ReadingTrailingHeaders => {
                    if let Some(pos) = find_crlf(&this.buf) {
                        let line = this.buf.split_to(pos + 2);
                        if pos == 0 {
                            this.state = DecoderState::Done;
                            if let Some(signed) = this.signed.as_mut() {
                                if let Err(e) = signed.finish_trailer() {
                                    return Poll::Ready(Some(Err(e)));
                                }
                            }
                            return Poll::Ready(None);
                        }
                        if let Some(signed) = this.signed.as_mut() {
                            if let Err(e) = signed.add_trailer_line(&line[..pos]) {
                                this.state = DecoderState::Done;
                                return Poll::Ready(Some(Err(e)));
                            }
                        }
                        this.record_trailer(&line[..pos]);
                        continue;
                    }
                    match Pin::new(&mut this.inner).poll_next(cx) {
                        Poll::Ready(Some(Ok(bytes))) => {
                            this.buf.extend_from_slice(&bytes);
                            continue;
                        }
                        Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                        Poll::Ready(None) => {
                            // A signed body is never accepted without its
                            // closing blank line: the trailer it ends is not
                            // verified until then.
                            if this.signed.is_some() {
                                this.state = DecoderState::Done;
                                return Poll::Ready(Some(Err(signature_mismatch())));
                            }
                            // Be lenient about a missing final \r\n: an
                            // unterminated last line still counts.
                            if !this.buf.is_empty() {
                                let line = this.buf.split().freeze();
                                this.record_trailer(&line);
                            }
                            this.state = DecoderState::Done;
                            return Poll::Ready(None);
                        }
                        Poll::Pending => return Poll::Pending,
                    }
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
                            this.state = DecoderState::Done;
                            if this.signed.is_some() {
                                // ...before the signed final chunk.
                                return Poll::Ready(Some(Err(signature_mismatch())));
                            }
                            // Be lenient: the final chunk's \r\n might be missing.
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

/// Parses a chunk header line: `<hex-size>[;chunk-signature=<signature>]`,
/// returning the size and the signature when there is one.
fn parse_chunk_header(line: &[u8]) -> Result<(usize, Option<String>), io::Error> {
    let line = std::str::from_utf8(line)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Non-UTF-8 chunk header"))?;

    // Size is before the first semicolon (if any).
    let (hex_part, extension) = match line.split_once(';') {
        Some((size, extension)) => (size.trim(), Some(extension)),
        None => (line.trim(), None),
    };

    let size = usize::from_str_radix(hex_part, 16)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "Invalid chunk size hex"))?;
    let signature = extension
        .and_then(|e| e.trim().strip_prefix("chunk-signature="))
        .map(|sig| sig.trim().to_string());
    Ok((size, signature))
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

    /// Decodes `raw` with a trailer sink, returning (data, trailers).
    async fn decode_with_trailers(chunks: Vec<&'static [u8]>) -> (Vec<u8>, Vec<(String, String)>) {
        let inner: ByteStream = Box::pin(tokio_stream::iter(
            chunks.into_iter().map(|c| Ok(Bytes::from_static(c))).collect::<Vec<_>>(),
        ));
        let sink: Trailers = Arc::new(Mutex::new(Vec::new()));
        let decoder = AwsChunkedDecoder::new(inner).with_trailers(sink.clone());
        let data = collect_stream(Box::pin(decoder)).await.unwrap();
        let trailers = sink.lock().unwrap().clone();
        (data, trailers)
    }

    #[tokio::test]
    async fn decode_unsigned_trailer() {
        // Exactly what botocore sends over TLS (STREAMING-UNSIGNED-PAYLOAD-TRAILER).
        let (data, trailers) = decode_with_trailers(vec![
            b"b\r\nhello world\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n",
        ])
        .await;
        assert_eq!(data, b"hello world");
        assert_eq!(
            trailers,
            vec![("x-amz-checksum-crc32".to_string(), "DUoRhQ==".to_string())]
        );
    }

    #[tokio::test]
    async fn decode_signed_trailer_keeps_every_line() {
        // STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER: signed chunks, then the
        // checksum and the trailer signature.
        let (data, trailers) = decode_with_trailers(vec![
            b"3;chunk-signature=aa\r\nfoo\r\n0;chunk-signature=bb\r\n",
            b"X-Amz-Checksum-SHA256: LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=\r\n",
            b"x-amz-trailer-signature:cc\r\n\r\n",
        ])
        .await;
        assert_eq!(data, b"foo");
        assert_eq!(
            trailers,
            vec![
                (
                    "x-amz-checksum-sha256".to_string(),
                    "LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ=".to_string()
                ),
                ("x-amz-trailer-signature".to_string(), "cc".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn decode_trailer_split_across_stream_chunks() {
        let (data, trailers) = decode_with_trailers(vec![
            b"5\r\nhel",
            b"lo\r\n0\r\nx-amz-check",
            b"sum-crc32c:abc=\r",
            b"\n\r\n",
        ])
        .await;
        assert_eq!(data, b"hello");
        assert_eq!(
            trailers,
            vec![("x-amz-checksum-crc32c".to_string(), "abc=".to_string())]
        );
    }

    #[tokio::test]
    async fn decode_trailer_without_final_crlf_is_accepted() {
        let (data, trailers) =
            decode_with_trailers(vec![b"2\r\nhi\r\n0\r\nx-amz-checksum-crc32:AAAAAA=="]).await;
        assert_eq!(data, b"hi");
        assert_eq!(
            trailers,
            vec![("x-amz-checksum-crc32".to_string(), "AAAAAA==".to_string())]
        );
    }

    #[tokio::test]
    async fn decode_without_trailers_leaves_sink_empty() {
        let (data, trailers) = decode_with_trailers(vec![b"3\r\nfoo\r\n0\r\n\r\n"]).await;
        assert_eq!(data, b"foo");
        assert!(trailers.is_empty());
    }

    #[tokio::test]
    async fn verified_body_stream_checks_a_trailer_checksum() {
        let raw = b"b\r\nhello world\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n";
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-content-sha256",
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER".parse().unwrap(),
        );
        headers.insert("x-amz-trailer", "x-amz-checksum-crc32".parse().unwrap());
        let (stream, check) =
            verified_body_stream(Body::from(raw.to_vec()), &headers, None, "/b/k", None).unwrap();
        assert_eq!(collect_stream(stream).await.unwrap(), b"hello world");
        // 5eb63bbbe01eeed093cb22bb8f5acdc3 = MD5("hello world")
        let (algo, value) = check.verify("5eb63bbbe01eeed093cb22bb8f5acdc3", "/b/k").unwrap();
        assert_eq!(algo.as_deref(), Some("CRC32"));
        assert_eq!(value.as_deref(), Some("DUoRhQ=="));
    }

    #[tokio::test]
    async fn verified_body_stream_rejects_tampered_trailer_body() {
        // Same trailer as above, but the body says "hello WORLD".
        let raw = b"b\r\nhello WORLD\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n";
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-amz-content-sha256",
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER".parse().unwrap(),
        );
        headers.insert("x-amz-trailer", "x-amz-checksum-crc32".parse().unwrap());
        let (stream, check) =
            verified_body_stream(Body::from(raw.to_vec()), &headers, None, "/b/k", None).unwrap();
        collect_stream(stream).await.unwrap();
        let err = check.verify("ignored-md5", "/b/k").unwrap_err();
        assert_eq!(err.code, arca_core::S3ErrorCode::BadDigest);
    }

    #[test]
    fn verified_body_stream_refuses_malformed_md5_up_front() {
        let mut headers = HeaderMap::new();
        headers.insert("content-md5", "garbage".parse().unwrap());
        let err = verified_body_stream(Body::from("x"), &headers, None, "/b/k", None)
            .err()
            .unwrap();
        assert_eq!(err.code, arca_core::S3ErrorCode::InvalidDigest);
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

    // -- Signed chunks (TD-045) --

    use arca_auth::{canonical_trailer, sha256_hex, ChunkVerifier, ParsedAuthorization};

    fn auth() -> ParsedAuthorization {
        ParsedAuthorization {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
            date: "20130524".to_string(),
            region: "us-east-1".to_string(),
            service: "s3".to_string(),
            signed_headers: vec![],
            signature: "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9".to_string(),
        }
    }

    fn verifier() -> ChunkVerifier {
        ChunkVerifier::new("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", &auth(), "20130524T000000Z")
    }

    /// Encodes `chunks` (and `trailer` headers, when given) as a signed
    /// aws-chunked body, chaining the signatures the way a client does.
    fn signed_body(chunks: &[&[u8]], trailer: Option<&[(&str, &str)]>) -> Vec<u8> {
        let mut v = verifier();
        let mut out = Vec::new();
        for chunk in chunks.iter().copied().chain(std::iter::once(&b""[..])) {
            let hash = sha256_hex(chunk);
            let sig = v.expected_chunk_signature(&hash);
            v.verify_chunk(&hash, &sig).unwrap();
            out.extend_from_slice(format!("{:x};chunk-signature={sig}\r\n", chunk.len()).as_bytes());
            out.extend_from_slice(chunk);
            out.extend_from_slice(b"\r\n");
        }
        // `out` ends with the final chunk's header, the CRLF that follows its
        // (empty) data being the blank line: drop it, the trailer lines (when
        // there are any) go before the blank line that closes the body.
        out.truncate(out.len() - 2);
        if let Some(trailer) = trailer {
            for (name, value) in trailer {
                out.extend_from_slice(format!("{name}:{value}\r\n").as_bytes());
            }
            let sig = v.expected_trailer_signature(&canonical_trailer(trailer.iter().copied()));
            out.extend_from_slice(format!("x-amz-trailer-signature:{sig}\r\n").as_bytes());
        }
        out.extend_from_slice(b"\r\n");
        out
    }

    /// Decodes `raw` in signed mode, fed `piece` bytes at a time.
    async fn decode_signed(
        raw: Vec<u8>,
        piece: usize,
        with_trailer: bool,
    ) -> (Result<Vec<u8>, io::Error>, Vec<(String, String)>) {
        let pieces: Vec<Result<Bytes, io::Error>> =
            raw.chunks(piece).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        let inner: ByteStream = Box::pin(tokio_stream::iter(pieces));
        let sink: Trailers = Arc::new(Mutex::new(Vec::new()));
        let decoder = AwsChunkedDecoder::new(inner)
            .with_trailers(sink.clone())
            .with_signature_verification(verifier(), with_trailer);
        let result = collect_stream(Box::pin(decoder)).await;
        let trailers = sink.lock().unwrap().clone();
        (result, trailers)
    }

    fn assert_signature_mismatch(result: Result<Vec<u8>, io::Error>) {
        let err = result.expect_err("the body must be refused");
        assert!(err.to_string().contains("SignatureDoesNotMatch"), "got: {err}");
    }

    #[tokio::test]
    async fn signed_body_is_decoded_and_verified() {
        let raw = signed_body(&[b"hello ", b"world"], None);
        let (result, _) = decode_signed(raw, usize::MAX, false).await;
        assert_eq!(result.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn signed_body_verifies_when_fed_one_byte_at_a_time() {
        let raw = signed_body(&[b"hello ", b"world"], None);
        let (result, _) = decode_signed(raw, 1, false).await;
        assert_eq!(result.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn signed_empty_body_is_accepted() {
        let (result, _) = decode_signed(signed_body(&[], None), usize::MAX, false).await;
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn signed_large_chunks_are_verified() {
        let big = vec![b'a'; 200 * 1024];
        let raw = signed_body(&[&big, b"tail"], None);
        let (result, _) = decode_signed(raw, 8192, false).await;
        assert_eq!(result.unwrap().len(), 200 * 1024 + 4);
    }

    #[tokio::test]
    async fn a_modified_data_byte_is_refused() {
        let mut raw = signed_body(&[b"hello ", b"world"], None);
        let pos = raw.windows(5).position(|w| w == b"world").unwrap();
        raw[pos] = b'W';
        let (result, _) = decode_signed(raw, usize::MAX, false).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_modified_chunk_signature_is_refused() {
        let mut raw = signed_body(&[b"hello"], None);
        let pos = raw.windows(17).position(|w| w == b";chunk-signature=").unwrap() + 17;
        raw[pos] = if raw[pos] == b'0' { b'1' } else { b'0' };
        let (result, _) = decode_signed(raw, usize::MAX, false).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_chunk_without_a_signature_is_refused() {
        let (result, _) = decode_signed(b"3\r\nfoo\r\n0\r\n\r\n".to_vec(), usize::MAX, false).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_body_cut_before_the_final_chunk_is_refused() {
        // Drop the whole final chunk: every chunk left carries a valid
        // signature, so only the missing terminator gives the truncation away.
        let raw = signed_body(&[b"hello ", b"world"], None);
        let cut = raw.windows(8).rposition(|w| w == b"0;chunk-").unwrap();
        let (result, _) = decode_signed(raw[..cut].to_vec(), usize::MAX, false).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_body_cut_anywhere_inside_a_chunk_is_refused() {
        let raw = signed_body(&[b"hello world"], None);
        let final_header = raw.windows(8).rposition(|w| w == b"0;chunk-").unwrap();
        let data = raw.windows(5).position(|w| w == b"hello").unwrap();
        for cut in [
            10,                  // inside the first chunk header
            data + 3,            // inside the chunk data
            final_header + 20,   // inside the final chunk header
        ] {
            let (result, _) = decode_signed(raw[..cut].to_vec(), usize::MAX, false).await;
            assert_signature_mismatch(result);
        }
    }

    #[tokio::test]
    async fn swapped_chunks_are_refused() {
        // Two valid chunks in the wrong order: each signature chains on the
        // one before it.
        let raw = signed_body(&[b"first", b"other"], None);
        let text = String::from_utf8(raw).unwrap();
        let parts: Vec<&str> = text.splitn(3, "\r\n").collect();
        // parts: [hdr1, "first", "<rest>"]; rebuild with the data swapped
        let swapped = format!("{}\r\nother\r\n{}", parts[0], parts[2].replacen("other", "first", 1));
        let (result, _) = decode_signed(swapped.into_bytes(), usize::MAX, false).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_signed_body_is_refused_under_another_chain() {
        let raw = signed_body(&[b"hello"], None);
        let pieces: Vec<Result<Bytes, io::Error>> = vec![Ok(Bytes::from(raw))];
        let inner: ByteStream = Box::pin(tokio_stream::iter(pieces));
        let other = ChunkVerifier::new("another-secret", &auth(), "20130524T000000Z");
        let decoder = AwsChunkedDecoder::new(inner).with_signature_verification(other, false);
        assert_signature_mismatch(collect_stream(Box::pin(decoder)).await);
    }

    #[tokio::test]
    async fn signed_trailer_is_verified_and_recorded() {
        let trailer = [("x-amz-checksum-crc32", "DUoRhQ==")];
        let raw = signed_body(&[b"hello world"], Some(&trailer));
        let (result, trailers) = decode_signed(raw, usize::MAX, true).await;
        assert_eq!(result.unwrap(), b"hello world");
        assert!(trailers.contains(&("x-amz-checksum-crc32".to_string(), "DUoRhQ==".to_string())));
    }

    #[tokio::test]
    async fn a_modified_trailer_value_is_refused() {
        let trailer = [("x-amz-checksum-crc32", "DUoRhQ==")];
        let raw = signed_body(&[b"hello world"], Some(&trailer));
        let text = String::from_utf8(raw).unwrap().replace("DUoRhQ==", "AAAAAA==");
        let (result, _) = decode_signed(text.into_bytes(), usize::MAX, true).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_missing_trailer_signature_is_refused() {
        let trailer = [("x-amz-checksum-crc32", "DUoRhQ==")];
        let raw = signed_body(&[b"hello world"], Some(&trailer));
        let text = String::from_utf8(raw).unwrap();
        let cut = text.find("x-amz-trailer-signature").unwrap();
        let unsigned = format!("{}\r\n", &text[..cut]);
        let (result, _) = decode_signed(unsigned.into_bytes(), usize::MAX, true).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_trailer_body_cut_before_the_blank_line_is_refused() {
        let trailer = [("x-amz-checksum-crc32", "DUoRhQ==")];
        let raw = signed_body(&[b"hello world"], Some(&trailer));
        let mut cut = raw.clone();
        cut.truncate(cut.len() - 2);
        let (result, _) = decode_signed(cut, usize::MAX, true).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn a_trailer_in_a_body_without_a_trailer_signature_mode_is_refused() {
        // `-PAYLOAD` (no `-TRAILER`): nothing authenticates a trailer line.
        let trailer = [("x-amz-checksum-crc32", "DUoRhQ==")];
        let raw = signed_body(&[b"hello world"], Some(&trailer));
        let (result, _) = decode_signed(raw, usize::MAX, false).await;
        assert_signature_mismatch(result);
    }

    #[tokio::test]
    async fn the_unsigned_decoder_stays_lenient() {
        // Signature extensions are stripped, not checked, without a verifier
        // (inter-node bodies, STREAMING-UNSIGNED-PAYLOAD-TRAILER).
        let raw = b"5;chunk-signature=junk\r\nhello\r\n0;chunk-signature=junk\r\n\r\n";
        let decoded = collect_stream(Box::pin(AwsChunkedDecoder::new(stream_from_bytes(raw))))
            .await
            .unwrap();
        assert_eq!(decoded, b"hello");
    }

    fn streaming_headers(mode: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-amz-content-sha256", mode.parse().unwrap());
        headers
    }

    #[tokio::test]
    async fn verified_body_stream_checks_signed_chunks() {
        let raw = signed_body(&[b"hello ", b"world"], None);
        let headers = streaming_headers("STREAMING-AWS4-HMAC-SHA256-PAYLOAD");
        let (stream, _) =
            verified_body_stream(Body::from(raw), &headers, None, "/b/k", Some(verifier())).unwrap();
        assert_eq!(collect_stream(stream).await.unwrap(), b"hello world");
    }

    #[tokio::test]
    async fn verified_body_stream_refuses_a_tampered_signed_body() {
        let mut raw = signed_body(&[b"hello world"], None);
        let pos = raw.windows(5).position(|w| w == b"hello").unwrap();
        raw[pos] = b'J';
        let headers = streaming_headers("STREAMING-AWS4-HMAC-SHA256-PAYLOAD");
        let (stream, _) =
            verified_body_stream(Body::from(raw), &headers, None, "/b/k", Some(verifier())).unwrap();
        assert_signature_mismatch(collect_stream(stream).await);
    }

    #[tokio::test]
    async fn verified_body_stream_checks_a_signed_trailer() {
        let trailer = [("x-amz-checksum-crc32", "DUoRhQ==")];
        let raw = signed_body(&[b"hello world"], Some(&trailer));
        let mut headers = streaming_headers("STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER");
        headers.insert("x-amz-trailer", "x-amz-checksum-crc32".parse().unwrap());
        let (stream, check) =
            verified_body_stream(Body::from(raw), &headers, None, "/b/k", Some(verifier())).unwrap();
        assert_eq!(collect_stream(stream).await.unwrap(), b"hello world");
        let (algo, _) = check.verify("5eb63bbbe01eeed093cb22bb8f5acdc3", "/b/k").unwrap();
        assert_eq!(algo.as_deref(), Some("CRC32"));
    }

    #[test]
    fn verified_body_stream_refuses_signed_chunks_without_a_seed() {
        // Presigned (query) auth has no seed signature to chain from.
        for mode in [
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER",
        ] {
            let err = verified_body_stream(
                Body::from("x"),
                &streaming_headers(mode),
                None,
                "/b/k",
                None,
            )
            .err()
            .unwrap();
            assert_eq!(err.code, arca_core::S3ErrorCode::SignatureDoesNotMatch, "{mode}");
        }
    }

    #[tokio::test]
    async fn verified_body_stream_leaves_unsigned_streaming_alone() {
        // STREAMING-UNSIGNED-PAYLOAD-TRAILER has no chunk signatures, even
        // when a verifier exists for the request.
        let raw = b"b\r\nhello world\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n";
        let mut headers = streaming_headers("STREAMING-UNSIGNED-PAYLOAD-TRAILER");
        headers.insert("x-amz-trailer", "x-amz-checksum-crc32".parse().unwrap());
        let (stream, _) = verified_body_stream(
            Body::from(raw.to_vec()),
            &headers,
            None,
            "/b/k",
            Some(verifier()),
        )
        .unwrap();
        assert_eq!(collect_stream(stream).await.unwrap(), b"hello world");
    }
}
