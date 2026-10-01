//! Request body integrity verification for PutObject and UploadPart.
//!
//! A request can declare the digest of its body in three independent ways,
//! all verified the way AWS S3 does:
//!
//! | Declared by | On mismatch | When malformed |
//! |-------------|-------------|----------------|
//! | `Content-MD5` (base64 MD5) | `BadDigest` | `InvalidDigest` |
//! | `x-amz-checksum-<algo>` header or aws-chunked trailer (base64) | `BadDigest` | `InvalidRequest` |
//! | hex `x-amz-content-sha256` | `XAmzContentSHA256Mismatch` | not verified (keywords such as `UNSIGNED-PAYLOAD`) |
//!
//! [`BodyIntegrity::from_headers`] parses the declarations before the body is
//! read, so a malformed one is refused without storing anything.
//! [`BodyIntegrity::wrap`] hashes the decoded body while it streams to the blob
//! store (only the digests the request asked for are computed; the MD5 is not
//! recomputed, the store already returns it as the ETag). Once the store has
//! written the blob, [`IntegrityCheck::verify`] compares everything; the
//! caller deletes the blob on error, before any sidecar or metadata row
//! exists.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use base64::Engine as _;
use bytes::Bytes;
use crc::{Crc, Table, CRC_32_ISCSI, CRC_32_ISO_HDLC, CRC_64_NVME};
use http::HeaderMap;
use sha1::Sha1;
use sha2::{Digest as _, Sha256};
use tokio_stream::Stream;

use arca_core::store::ByteStream;
use arca_core::{S3Error, S3ErrorCode};

static CRC32: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISO_HDLC);
static CRC32C: Crc<u32, Table<16>> = Crc::<u32, Table<16>>::new(&CRC_32_ISCSI);
static CRC64NVME: Crc<u64, Table<16>> = Crc::<u64, Table<16>>::new(&CRC_64_NVME);

/// Trailing headers of an aws-chunked body (`name` lowercased), filled in by
/// the chunked decoder once it reaches the end of the body.
pub type Trailers = Arc<Mutex<Vec<(String, String)>>>;

/// The `x-amz-checksum-*` algorithms S3 supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumAlgorithm {
    Crc32,
    Crc32c,
    Crc64Nvme,
    Sha1,
    Sha256,
}

impl ChecksumAlgorithm {
    pub const ALL: [ChecksumAlgorithm; 5] = [
        ChecksumAlgorithm::Crc32,
        ChecksumAlgorithm::Crc32c,
        ChecksumAlgorithm::Crc64Nvme,
        ChecksumAlgorithm::Sha1,
        ChecksumAlgorithm::Sha256,
    ];

    /// Algorithm name as S3 spells it (`CRC32`, ..., `SHA256`).
    pub fn name(self) -> &'static str {
        match self {
            ChecksumAlgorithm::Crc32 => "CRC32",
            ChecksumAlgorithm::Crc32c => "CRC32C",
            ChecksumAlgorithm::Crc64Nvme => "CRC64NVME",
            ChecksumAlgorithm::Sha1 => "SHA1",
            ChecksumAlgorithm::Sha256 => "SHA256",
        }
    }

    /// The header carrying this algorithm's value (`x-amz-checksum-crc32`, ...).
    pub fn header_name(self) -> &'static str {
        match self {
            ChecksumAlgorithm::Crc32 => "x-amz-checksum-crc32",
            ChecksumAlgorithm::Crc32c => "x-amz-checksum-crc32c",
            ChecksumAlgorithm::Crc64Nvme => "x-amz-checksum-crc64nvme",
            ChecksumAlgorithm::Sha1 => "x-amz-checksum-sha1",
            ChecksumAlgorithm::Sha256 => "x-amz-checksum-sha256",
        }
    }

    fn from_header_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|a| a.header_name().eq_ignore_ascii_case(name))
    }

    /// Length in bytes of the raw (base64-decoded) digest.
    fn digest_len(self) -> usize {
        match self {
            ChecksumAlgorithm::Crc32 | ChecksumAlgorithm::Crc32c => 4,
            ChecksumAlgorithm::Crc64Nvme => 8,
            ChecksumAlgorithm::Sha1 => 20,
            ChecksumAlgorithm::Sha256 => 32,
        }
    }
}

/// Incremental hasher for one [`ChecksumAlgorithm`].
pub enum ChecksumHasher {
    Crc32(crc::Digest<'static, u32, Table<16>>),
    Crc32c(crc::Digest<'static, u32, Table<16>>),
    Crc64Nvme(crc::Digest<'static, u64, Table<16>>),
    Sha1(Sha1),
    Sha256(Sha256),
}

impl ChecksumHasher {
    pub fn new(algorithm: ChecksumAlgorithm) -> Self {
        match algorithm {
            ChecksumAlgorithm::Crc32 => ChecksumHasher::Crc32(CRC32.digest()),
            ChecksumAlgorithm::Crc32c => ChecksumHasher::Crc32c(CRC32C.digest()),
            ChecksumAlgorithm::Crc64Nvme => ChecksumHasher::Crc64Nvme(CRC64NVME.digest()),
            ChecksumAlgorithm::Sha1 => ChecksumHasher::Sha1(Sha1::new()),
            ChecksumAlgorithm::Sha256 => ChecksumHasher::Sha256(Sha256::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            ChecksumHasher::Crc32(d) | ChecksumHasher::Crc32c(d) => d.update(data),
            ChecksumHasher::Crc64Nvme(d) => d.update(data),
            ChecksumHasher::Sha1(h) => h.update(data),
            ChecksumHasher::Sha256(h) => h.update(data),
        }
    }

    /// Raw digest bytes; CRCs are big-endian, as S3 encodes them.
    pub fn finalize(self) -> Vec<u8> {
        match self {
            ChecksumHasher::Crc32(d) | ChecksumHasher::Crc32c(d) => {
                d.finalize().to_be_bytes().to_vec()
            }
            ChecksumHasher::Crc64Nvme(d) => d.finalize().to_be_bytes().to_vec(),
            ChecksumHasher::Sha1(h) => h.finalize().to_vec(),
            ChecksumHasher::Sha256(h) => h.finalize().to_vec(),
        }
    }
}

/// Where the client's `x-amz-checksum-*` value is.
#[derive(Debug)]
enum ChecksumSource {
    /// In a request header, already decoded.
    Header(Vec<u8>),
    /// In an aws-chunked trailer named by `x-amz-trailer`, known only once
    /// the body has been read.
    Trailer,
}

/// The integrity declarations of one request, parsed before its body is read.
#[derive(Debug, Default)]
pub struct BodyIntegrity {
    content_md5: Option<[u8; 16]>,
    content_sha256: Option<[u8; 32]>,
    checksum: Option<(ChecksumAlgorithm, ChecksumSource)>,
    /// `x-amz-checksum-algorithm` without any value: recorded on the object
    /// (as before), nothing to verify.
    declared_algorithm: Option<String>,
}

fn invalid_request(message: String, resource: &str) -> S3Error {
    S3Error::with_message(S3ErrorCode::InvalidRequest, message, resource)
}

/// Decodes a base64 checksum value and checks its length for `algorithm`.
fn decode_checksum(algorithm: ChecksumAlgorithm, value: &str) -> Option<Vec<u8>> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .ok()?;
    (raw.len() == algorithm.digest_len()).then_some(raw)
}

impl BodyIntegrity {
    /// Parses `Content-MD5`, `x-amz-checksum-*`, `x-amz-trailer` and
    /// `x-amz-content-sha256`. Fails with the S3 error AWS returns for a
    /// malformed or contradictory declaration.
    pub fn from_headers(headers: &HeaderMap, resource: &str) -> Result<Self, S3Error> {
        let mut integrity = BodyIntegrity::default();

        if let Some(value) = headers.get("content-md5") {
            let raw = value
                .to_str()
                .ok()
                .and_then(|v| base64::engine::general_purpose::STANDARD.decode(v.trim()).ok())
                .and_then(|raw| <[u8; 16]>::try_from(raw).ok())
                .ok_or_else(|| S3Error::new(S3ErrorCode::InvalidDigest, resource))?;
            integrity.content_md5 = Some(raw);
        }

        if let Some(value) = headers
            .get("x-amz-content-sha256")
            .and_then(|v| v.to_str().ok())
        {
            // Only a literal digest can be compared; UNSIGNED-PAYLOAD and the
            // STREAMING-* keywords carry no body hash.
            if value.len() == 64 {
                if let Ok(raw) = hex::decode(value) {
                    integrity.content_sha256 = raw.try_into().ok();
                }
            }
        }

        let mut declared: Vec<(ChecksumAlgorithm, ChecksumSource)> = Vec::new();
        for algorithm in ChecksumAlgorithm::ALL {
            if let Some(value) = headers.get(algorithm.header_name()) {
                let raw = value
                    .to_str()
                    .ok()
                    .and_then(|v| decode_checksum(algorithm, v))
                    .ok_or_else(|| {
                        invalid_request(
                            format!("Value for {} header is invalid.", algorithm.header_name()),
                            resource,
                        )
                    })?;
                declared.push((algorithm, ChecksumSource::Header(raw)));
            }
        }
        if let Some(trailer) = headers.get("x-amz-trailer").and_then(|v| v.to_str().ok()) {
            for name in trailer.split(',').map(str::trim) {
                if !name.to_ascii_lowercase().starts_with("x-amz-checksum-") {
                    continue;
                }
                let algorithm = ChecksumAlgorithm::from_header_name(name).ok_or_else(|| {
                    invalid_request(
                        format!("The value specified in the x-amz-trailer header is not supported: {name}"),
                        resource,
                    )
                })?;
                declared.push((algorithm, ChecksumSource::Trailer));
            }
        }
        if declared.len() > 1 {
            return Err(invalid_request(
                "Expecting a single x-amz-checksum- header. Multiple checksum Types are not allowed."
                    .to_string(),
                resource,
            ));
        }
        integrity.checksum = declared.pop();

        if integrity.checksum.is_none() {
            integrity.declared_algorithm = headers
                .get("x-amz-checksum-algorithm")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_uppercase());
        }

        Ok(integrity)
    }

    /// Wraps the decoded body so that the digests to verify are computed as
    /// it streams. `trailers` is the decoder's trailer sink (aws-chunked
    /// bodies only).
    pub fn wrap(self, stream: ByteStream, trailers: Option<Trailers>) -> (ByteStream, IntegrityCheck) {
        let checksum = self
            .checksum
            .as_ref()
            .map(|(algorithm, _)| ChecksumHasher::new(*algorithm));
        // A SHA256 checksum doubles as the x-amz-content-sha256 digest.
        let checksum_is_sha256 = matches!(self.checksum, Some((ChecksumAlgorithm::Sha256, _)));
        let content_sha256 = (self.content_sha256.is_some() && !checksum_is_sha256).then(Sha256::new);

        let state = Arc::new(Mutex::new(HashState {
            content_sha256,
            checksum,
        }));
        let needs_hashing = {
            let s = state.lock().expect("hash state lock");
            s.content_sha256.is_some() || s.checksum.is_some()
        };
        let stream = if needs_hashing {
            Box::pin(HashingStream {
                inner: stream,
                state: state.clone(),
            }) as ByteStream
        } else {
            stream
        };
        (
            stream,
            IntegrityCheck {
                spec: self,
                state,
                trailers,
            },
        )
    }
}

struct HashState {
    content_sha256: Option<Sha256>,
    checksum: Option<ChecksumHasher>,
}

/// Feeds every chunk of the body into the hashers of a [`HashState`].
struct HashingStream {
    inner: ByteStream,
    state: Arc<Mutex<HashState>>,
}

impl Stream for HashingStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let polled = self.inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(Ok(ref chunk))) = polled {
            let mut state = self.state.lock().expect("hash state lock");
            if let Some(h) = state.content_sha256.as_mut() {
                h.update(chunk);
            }
            if let Some(h) = state.checksum.as_mut() {
                h.update(chunk);
            }
        }
        polled
    }
}

/// The checksum to record on the object: `(algorithm, base64 value)`, the
/// shape of `ObjectRecord::{checksum_algorithm, checksum_value}`.
pub type VerifiedChecksum = (Option<String>, Option<String>);

/// Verification state of one request body; consumed by [`Self::verify`].
pub struct IntegrityCheck {
    spec: BodyIntegrity,
    state: Arc<Mutex<HashState>>,
    trailers: Option<Trailers>,
}

impl IntegrityCheck {
    /// Compares the declared digests with the body that was stored.
    /// `etag` is the hex MD5 of the plaintext as returned by the blob store.
    ///
    /// Must be called after the body stream has been fully consumed. On
    /// success returns the checksum to record on the object.
    pub fn verify(self, etag: &str, resource: &str) -> Result<VerifiedChecksum, S3Error> {
        if let Some(expected) = self.spec.content_md5 {
            if !hex::encode(expected).eq_ignore_ascii_case(etag) {
                return Err(S3Error::new(S3ErrorCode::BadDigest, resource));
            }
        }

        let (content_sha256, checksum) = {
            let mut state = self.state.lock().expect("hash state lock");
            (
                state.content_sha256.take().map(|h| h.finalize().to_vec()),
                state.checksum.take().map(ChecksumHasher::finalize),
            )
        };

        if let Some(expected) = self.spec.content_sha256 {
            let computed = match (&self.spec.checksum, &checksum) {
                (Some((ChecksumAlgorithm::Sha256, _)), Some(digest)) => Some(digest.clone()),
                _ => content_sha256,
            };
            if computed.as_deref() != Some(expected.as_slice()) {
                return Err(S3Error::new(S3ErrorCode::XAmzContentSHA256Mismatch, resource));
            }
        }

        let Some((algorithm, source)) = self.spec.checksum else {
            return Ok((self.spec.declared_algorithm, None));
        };
        let expected = match source {
            ChecksumSource::Header(raw) => raw,
            ChecksumSource::Trailer => {
                let value = self
                    .trailers
                    .as_ref()
                    .and_then(|t| {
                        t.lock()
                            .expect("trailers lock")
                            .iter()
                            .find(|(name, _)| name == algorithm.header_name())
                            .map(|(_, value)| value.clone())
                    })
                    .ok_or_else(|| {
                        invalid_request(
                            format!(
                                "x-amz-trailer declares {} but the body has no such trailing header.",
                                algorithm.header_name()
                            ),
                            resource,
                        )
                    })?;
                decode_checksum(algorithm, &value).ok_or_else(|| {
                    invalid_request(
                        format!("Value for {} trailing header is invalid.", algorithm.header_name()),
                        resource,
                    )
                })?
            }
        };
        if checksum.as_deref() != Some(expected.as_slice()) {
            return Err(S3Error::with_message(
                S3ErrorCode::BadDigest,
                format!(
                    "The {} you specified did not match the calculated checksum.",
                    algorithm.name()
                ),
                resource,
            ));
        }
        Ok((
            Some(algorithm.name().to_string()),
            Some(base64::engine::general_purpose::STANDARD.encode(&expected)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

    const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

    fn checksum_b64(algorithm: ChecksumAlgorithm, data: &[u8]) -> String {
        let mut h = ChecksumHasher::new(algorithm);
        h.update(data);
        B64.encode(h.finalize())
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.append(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        map
    }

    fn md5_hex(data: &[u8]) -> String {
        use md5::{Digest, Md5};
        hex::encode(Md5::digest(data))
    }

    /// Streams `data` (split in two chunks) through the integrity wrapper,
    /// then verifies it as a handler would.
    async fn run(
        h: &HeaderMap,
        data: &[u8],
        trailers: Option<Trailers>,
    ) -> Result<VerifiedChecksum, S3Error> {
        let integrity = BodyIntegrity::from_headers(h, "/b/k")?;
        let mid = data.len() / 2;
        let chunks = vec![
            Ok(Bytes::copy_from_slice(&data[..mid])),
            Ok(Bytes::copy_from_slice(&data[mid..])),
        ];
        let (stream, check) = integrity.wrap(Box::pin(tokio_stream::iter(chunks)), trailers);
        let mut stream = stream;
        let mut out = Vec::new();
        while let Some(c) = stream.next().await {
            out.extend_from_slice(&c.unwrap());
        }
        assert_eq!(out, data, "the wrapper must not alter the body");
        check.verify(&md5_hex(data), "/b/k")
    }

    // -- Algorithms, against published vectors --

    #[test]
    fn crc_check_values() {
        // The catalogue "check" value: CRC of b"123456789".
        let digest = |a| {
            let mut h = ChecksumHasher::new(a);
            h.update(b"123456789");
            h.finalize()
        };
        assert_eq!(digest(ChecksumAlgorithm::Crc32), 0xcbf4_3926u32.to_be_bytes());
        assert_eq!(digest(ChecksumAlgorithm::Crc32c), 0xe306_9283u32.to_be_bytes());
        assert_eq!(
            digest(ChecksumAlgorithm::Crc64Nvme),
            0xae8b_1486_0a79_9888u64.to_be_bytes()
        );
    }

    #[test]
    fn checksums_match_known_s3_values() {
        // What boto3 sends for b"hello world" (x-amz-checksum-crc32: DUoRhQ==).
        assert_eq!(checksum_b64(ChecksumAlgorithm::Crc32, b"hello world"), "DUoRhQ==");
        // Ceph s3-tests vectors: 1024 bytes of 'A'.
        let a1024 = vec![b'A'; 1024];
        assert_eq!(
            checksum_b64(ChecksumAlgorithm::Sha256, &a1024),
            "arcu6553sHVAiX4MjW0j7I7vD4w6R+Gz9Ok0Q9lTa+0="
        );
        assert_eq!(checksum_b64(ChecksumAlgorithm::Crc64Nvme, &a1024), "Qeh8oXvGiSo=");
        // SHA1("abc"), FIPS 180 vector.
        assert_eq!(
            hex::encode(B64.decode(checksum_b64(ChecksumAlgorithm::Sha1, b"abc")).unwrap()),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn hashing_in_pieces_equals_one_shot() {
        let data = b"The quick brown fox jumps over the lazy dog";
        for a in ChecksumAlgorithm::ALL {
            let mut h = ChecksumHasher::new(a);
            for piece in data.chunks(7) {
                h.update(piece);
            }
            assert_eq!(B64.encode(h.finalize()), checksum_b64(a, data), "{a:?}");
        }
    }

    // -- No declaration --

    #[tokio::test]
    async fn no_integrity_headers_accepts_anything() {
        let h = headers(&[("x-amz-content-sha256", "UNSIGNED-PAYLOAD")]);
        assert_eq!(run(&h, b"data", None).await.unwrap(), (None, None));
    }

    #[tokio::test]
    async fn algorithm_without_value_is_recorded_unverified() {
        let h = headers(&[("x-amz-checksum-algorithm", "crc32")]);
        assert_eq!(
            run(&h, b"data", None).await.unwrap(),
            (Some("CRC32".to_string()), None)
        );
    }

    // -- Content-MD5 --

    #[tokio::test]
    async fn content_md5_match() {
        let md5 = B64.encode(hex::decode(md5_hex(b"hello")).unwrap());
        let h = headers(&[("content-md5", &md5)]);
        assert!(run(&h, b"hello", None).await.is_ok());
    }

    #[tokio::test]
    async fn content_md5_mismatch_is_bad_digest() {
        let md5 = B64.encode(hex::decode(md5_hex(b"hello")).unwrap());
        let h = headers(&[("content-md5", &md5)]);
        let err = run(&h, b"HELLO", None).await.unwrap_err();
        assert_eq!(err.code, S3ErrorCode::BadDigest);
        assert_eq!(err.message, "The Content-MD5 you specified did not match what we received.");
    }

    #[test]
    fn malformed_content_md5_is_invalid_digest() {
        for bad in ["not base64!", "", "AAAA", "XUFAKrxLKna5cZ2REBfFkgAA"] {
            let h = headers(&[("content-md5", bad)]);
            let err = BodyIntegrity::from_headers(&h, "/b/k").unwrap_err();
            assert_eq!(err.code, S3ErrorCode::InvalidDigest, "{bad:?}");
        }
    }

    // -- x-amz-content-sha256 --

    #[tokio::test]
    async fn hex_content_sha256_match() {
        let sha = hex::encode(Sha256::digest(b"payload"));
        let h = headers(&[("x-amz-content-sha256", &sha)]);
        assert!(run(&h, b"payload", None).await.is_ok());
    }

    #[tokio::test]
    async fn hex_content_sha256_mismatch() {
        let sha = hex::encode(Sha256::digest(b"payload"));
        let h = headers(&[("x-amz-content-sha256", &sha)]);
        let err = run(&h, b"PAYLOAD", None).await.unwrap_err();
        assert_eq!(err.code, S3ErrorCode::XAmzContentSHA256Mismatch);
    }

    #[tokio::test]
    async fn content_sha256_keywords_are_not_verified() {
        for kw in [
            "UNSIGNED-PAYLOAD",
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
        ] {
            let h = headers(&[("x-amz-content-sha256", kw)]);
            assert!(run(&h, b"anything", None).await.is_ok(), "{kw}");
        }
    }

    #[tokio::test]
    async fn sha256_checksum_and_content_sha256_together() {
        // What boto3 sends over HTTP with ChecksumAlgorithm=SHA256: one
        // digest serves both checks.
        let data = b"both";
        let h = headers(&[
            ("x-amz-content-sha256", &hex::encode(Sha256::digest(data))),
            ("x-amz-checksum-sha256", &checksum_b64(ChecksumAlgorithm::Sha256, data)),
        ]);
        assert!(run(&h, data, None).await.is_ok());

        let h = headers(&[
            ("x-amz-content-sha256", &hex::encode(Sha256::digest(b"other"))),
            ("x-amz-checksum-sha256", &checksum_b64(ChecksumAlgorithm::Sha256, data)),
        ]);
        let err = run(&h, data, None).await.unwrap_err();
        assert_eq!(err.code, S3ErrorCode::XAmzContentSHA256Mismatch);
    }

    // -- x-amz-checksum-* headers --

    #[tokio::test]
    async fn every_algorithm_header_match_is_recorded() {
        let data = b"checksummed body";
        for a in ChecksumAlgorithm::ALL {
            let value = checksum_b64(a, data);
            let h = headers(&[(a.header_name(), &value)]);
            assert_eq!(
                run(&h, data, None).await.unwrap(),
                (Some(a.name().to_string()), Some(value)),
                "{a:?}"
            );
        }
    }

    #[tokio::test]
    async fn every_algorithm_header_mismatch_is_bad_digest() {
        for a in ChecksumAlgorithm::ALL {
            let value = checksum_b64(a, b"expected body");
            let h = headers(&[(a.header_name(), &value)]);
            let err = run(&h, b"another body", None).await.unwrap_err();
            assert_eq!(err.code, S3ErrorCode::BadDigest, "{a:?}");
            assert_eq!(
                err.message,
                format!("The {} you specified did not match the calculated checksum.", a.name())
            );
        }
    }

    #[test]
    fn malformed_checksum_header_is_invalid_request() {
        // "bad" is not base64; "AAAA" decodes to 3 bytes, wrong for every algorithm.
        for a in ChecksumAlgorithm::ALL {
            for bad in ["bad", "AAAA"] {
                let h = headers(&[(a.header_name(), bad)]);
                let err = BodyIntegrity::from_headers(&h, "/b/k").unwrap_err();
                assert_eq!(err.code, S3ErrorCode::InvalidRequest, "{a:?} {bad}");
                assert_eq!(
                    err.message,
                    format!("Value for {} header is invalid.", a.header_name())
                );
            }
        }
    }

    #[test]
    fn two_checksum_headers_are_rejected() {
        let h = headers(&[
            ("x-amz-checksum-crc32", &checksum_b64(ChecksumAlgorithm::Crc32, b"x")),
            ("x-amz-checksum-sha1", &checksum_b64(ChecksumAlgorithm::Sha1, b"x")),
        ]);
        let err = BodyIntegrity::from_headers(&h, "/b/k").unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRequest);
    }

    // -- Trailing checksums (aws-chunked) --

    fn trailers(pairs: &[(&str, &str)]) -> Trailers {
        Arc::new(Mutex::new(
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ))
    }

    #[tokio::test]
    async fn trailer_checksum_match_is_recorded() {
        let h = headers(&[
            ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ]);
        let t = trailers(&[("x-amz-checksum-crc32", "DUoRhQ==")]);
        assert_eq!(
            run(&h, b"hello world", Some(t)).await.unwrap(),
            (Some("CRC32".to_string()), Some("DUoRhQ==".to_string()))
        );
    }

    #[tokio::test]
    async fn trailer_checksum_mismatch_is_bad_digest() {
        let h = headers(&[("x-amz-trailer", "x-amz-checksum-crc32c")]);
        let value = checksum_b64(ChecksumAlgorithm::Crc32c, b"original");
        let t = trailers(&[("x-amz-checksum-crc32c", &value)]);
        let err = run(&h, b"tampered", Some(t)).await.unwrap_err();
        assert_eq!(err.code, S3ErrorCode::BadDigest);
    }

    #[tokio::test]
    async fn missing_or_malformed_trailer_is_invalid_request() {
        let h = headers(&[("x-amz-trailer", "x-amz-checksum-crc32")]);
        let err = run(&h, b"data", Some(trailers(&[]))).await.unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRequest);

        let t = trailers(&[("x-amz-checksum-crc32", "nope")]);
        let err = run(&h, b"data", Some(t)).await.unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn unsupported_trailer_algorithm_is_rejected() {
        let h = headers(&[("x-amz-trailer", "x-amz-checksum-md4")]);
        let err = BodyIntegrity::from_headers(&h, "/b/k").unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRequest);
    }

    #[test]
    fn header_and_trailer_checksum_together_are_rejected() {
        let h = headers(&[
            ("x-amz-checksum-crc32", "DUoRhQ=="),
            ("x-amz-trailer", "x-amz-checksum-crc32"),
        ]);
        let err = BodyIntegrity::from_headers(&h, "/b/k").unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRequest);
    }

    #[tokio::test]
    async fn content_md5_is_checked_with_a_trailer_checksum() {
        // boto3 over TLS with ContentMD5: Content-MD5 header + CRC32 trailer.
        let md5 = B64.encode(hex::decode(md5_hex(b"other")).unwrap());
        let h = headers(&[("content-md5", &md5), ("x-amz-trailer", "x-amz-checksum-crc32")]);
        let t = trailers(&[("x-amz-checksum-crc32", "DUoRhQ==")]);
        let err = run(&h, b"hello world", Some(t)).await.unwrap_err();
        assert_eq!(err.code, S3ErrorCode::BadDigest);
    }
}
