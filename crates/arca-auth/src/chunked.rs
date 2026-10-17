//! Signature chain of an `aws-chunked` upload (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD`
//! and its `-TRAILER` variant).
//!
//! The request's own signature (the "seed") covers the headers. Every chunk
//! then carries a signature over its data and the signature before it, and the
//! trailer (when there is one) a signature over its headers and the last chunk
//! signature, so the whole body is authenticated as a chain:
//!
//! ```text
//! AWS4-HMAC-SHA256-PAYLOAD\n<datetime>\n<scope>\n<previous signature>\n<hex SHA-256 of "">\n<hex SHA-256 of the chunk>
//! AWS4-HMAC-SHA256-TRAILER\n<datetime>\n<scope>\n<previous signature>\n<hex SHA-256 of the trailer headers>
//! ```
//!
//! This module only does the arithmetic; framing the body and hashing the
//! chunk data is the caller's job.

use std::fmt;

use subtle::ConstantTimeEq;

use crate::error::AuthError;
use crate::parse::ParsedAuthorization;
use crate::sigv4::{compute_signature, hex_sha256, signing_key};

/// Verifies the chunk and trailer signatures of one `aws-chunked` request.
///
/// Built from the secret key and the verified `Authorization` header, it holds
/// the derived signing key (never the secret) and the signature of the last
/// element verified, starting from the request's seed signature.
#[derive(Clone)]
pub struct ChunkVerifier {
    signing_key: Vec<u8>,
    datetime: String,
    scope: String,
    previous_signature: String,
}

impl fmt::Debug for ChunkVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChunkVerifier")
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl ChunkVerifier {
    /// `auth` is the already verified `Authorization` header: its signature is
    /// the seed of the chain. `request_datetime` is the `x-amz-date` value.
    pub fn new(secret_access_key: &str, auth: &ParsedAuthorization, request_datetime: &str) -> Self {
        Self {
            signing_key: signing_key(secret_access_key, &auth.date, &auth.region, &auth.service),
            datetime: request_datetime.to_string(),
            scope: format!("{}/{}/{}/aws4_request", auth.date, auth.region, auth.service),
            previous_signature: auth.signature.clone(),
        }
    }

    /// The signature the next chunk must carry, given the lowercase hex
    /// SHA-256 of its data (`hex_sha256("")` for the final, empty chunk).
    pub fn expected_chunk_signature(&self, chunk_sha256_hex: &str) -> String {
        let sts = format!(
            "AWS4-HMAC-SHA256-PAYLOAD\n{}\n{}\n{}\n{}\n{}",
            self.datetime,
            self.scope,
            self.previous_signature,
            hex_sha256(b""),
            chunk_sha256_hex
        );
        compute_signature(&self.signing_key, &sts)
    }

    /// The signature the trailer must carry, given the canonical trailer
    /// headers (see [`canonical_trailer`]).
    pub fn expected_trailer_signature(&self, canonical_trailer: &[u8]) -> String {
        let sts = format!(
            "AWS4-HMAC-SHA256-TRAILER\n{}\n{}\n{}\n{}",
            self.datetime,
            self.scope,
            self.previous_signature,
            hex_sha256(canonical_trailer)
        );
        compute_signature(&self.signing_key, &sts)
    }

    /// Checks the signature of the next chunk and, on success, makes it the
    /// previous one. On failure the chain does not advance.
    pub fn verify_chunk(&mut self, chunk_sha256_hex: &str, signature: &str) -> Result<(), AuthError> {
        let expected = self.expected_chunk_signature(chunk_sha256_hex);
        self.advance(expected, signature)
    }

    /// Checks the trailer signature; see [`ChunkVerifier::verify_chunk`].
    pub fn verify_trailer(&mut self, canonical_trailer: &[u8], signature: &str) -> Result<(), AuthError> {
        let expected = self.expected_trailer_signature(canonical_trailer);
        self.advance(expected, signature)
    }

    fn advance(&mut self, expected: String, provided: &str) -> Result<(), AuthError> {
        if expected.as_bytes().ct_eq(provided.as_bytes()).into() {
            self.previous_signature = expected;
            Ok(())
        } else {
            Err(AuthError::SignatureDoesNotMatch)
        }
    }
}

/// The text a trailer signature covers: every trailer header except
/// `x-amz-trailer-signature`, as `name:value\n` with a lowercase name and a
/// trimmed value, in the order they were sent.
pub fn canonical_trailer<'a>(headers: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in headers {
        let name = name.trim().to_ascii_lowercase();
        if name == "x-amz-trailer-signature" {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.push(b':');
        out.extend_from_slice(value.trim().as_bytes());
        out.push(b'\n');
    }
    out
}

/// Lowercase hex SHA-256, the form [`ChunkVerifier::verify_chunk`] takes.
pub fn sha256_hex(data: &[u8]) -> String {
    hex_sha256(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The example of "Signature Calculations for the Authorization Header:
    // Transferring Payload in Multiple Chunks (Chunked Upload)" in the AWS S3
    // documentation: 66560 bytes of 'a' in chunks of 64 KiB and 1 KiB.
    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    const DATETIME: &str = "20130524T000000Z";
    const SEED: &str = "4f232c4386841ef735655705268965c44a0e4690baa4adea153f7db9fa80a0a9";
    const CHUNK1: &str = "ad80c730a21e5b8d04586a2213dd63b9a0e99e0e2307b0ade35a65485a288648";
    const CHUNK2: &str = "0055627c9e194cb4542bae2aa5492e3c1575bbb81b612b7d234b86a503ef5497";
    const FINAL: &str = "b6c6ea8a5354eaf15b3cb7646744f4275b71ea724fed81ceb9323e279d449df9";

    fn auth() -> ParsedAuthorization {
        ParsedAuthorization {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".to_string(),
            date: "20130524".to_string(),
            region: "us-east-1".to_string(),
            service: "s3".to_string(),
            signed_headers: vec![],
            signature: SEED.to_string(),
        }
    }

    fn verifier() -> ChunkVerifier {
        ChunkVerifier::new(SECRET, &auth(), DATETIME)
    }

    #[test]
    fn aws_example_chunks_verify_in_order() {
        let mut v = verifier();
        v.verify_chunk(&sha256_hex(&[b'a'; 65536]), CHUNK1).unwrap();
        v.verify_chunk(&sha256_hex(&[b'a'; 1024]), CHUNK2).unwrap();
        v.verify_chunk(&sha256_hex(b""), FINAL).unwrap();
    }

    #[test]
    fn a_chunk_signed_for_other_data_is_refused() {
        let mut v = verifier();
        let err = v.verify_chunk(&sha256_hex(&[b'b'; 65536]), CHUNK1).unwrap_err();
        assert_eq!(err, AuthError::SignatureDoesNotMatch);
    }

    #[test]
    fn chunks_cannot_be_reordered_dropped_or_replayed() {
        let first = sha256_hex(&[b'a'; 65536]);
        let second = sha256_hex(&[b'a'; 1024]);
        // The second chunk right after the seed: its signature chains on the first.
        assert!(verifier().verify_chunk(&second, CHUNK2).is_err());
        // The first chunk twice.
        let mut v = verifier();
        v.verify_chunk(&first, CHUNK1).unwrap();
        assert!(v.verify_chunk(&first, CHUNK1).is_err());
        // The final chunk without the data chunks before it (truncation then forgery).
        assert!(verifier().verify_chunk(&sha256_hex(b""), FINAL).is_err());
    }

    #[test]
    fn a_failed_chunk_does_not_advance_the_chain() {
        let mut v = verifier();
        assert!(v.verify_chunk(&sha256_hex(b"wrong"), CHUNK1).is_err());
        // The right chunk still verifies afterwards.
        v.verify_chunk(&sha256_hex(&[b'a'; 65536]), CHUNK1).unwrap();
    }

    #[test]
    fn a_signature_of_the_wrong_length_or_case_is_refused() {
        let hash = sha256_hex(&[b'a'; 65536]);
        let mut v = verifier();
        assert!(v.verify_chunk(&hash, "").is_err());
        assert!(v.verify_chunk(&hash, &CHUNK1[..63]).is_err());
        assert!(v.verify_chunk(&hash, &CHUNK1.to_uppercase()).is_err());
    }

    #[test]
    fn another_secret_or_date_does_not_verify() {
        let hash = sha256_hex(&[b'a'; 65536]);
        let mut other_secret = ChunkVerifier::new("not-the-secret", &auth(), DATETIME);
        assert!(other_secret.verify_chunk(&hash, CHUNK1).is_err());
        let mut other_date = ChunkVerifier::new(SECRET, &auth(), "20130524T000001Z");
        assert!(other_date.verify_chunk(&hash, CHUNK1).is_err());
    }

    #[test]
    fn trailer_signature_chains_on_the_last_chunk() {
        let mut v = verifier();
        let canonical = canonical_trailer([("X-Amz-Checksum-CRC32", " DUoRhQ== ")]);
        assert_eq!(canonical, b"x-amz-checksum-crc32:DUoRhQ==\n");

        // The signature is a pure function of the chain, so the verifier
        // accepts what it would itself have produced and nothing else.
        v.verify_chunk(&sha256_hex(&[b'a'; 65536]), CHUNK1).unwrap();
        let sig = v.expected_trailer_signature(&canonical);
        assert!(v.verify_trailer(&canonical, &"0".repeat(64)).is_err());
        v.verify_trailer(&canonical, &sig).unwrap();
        // ...and a trailer for other values does not.
        let mut v2 = verifier();
        v2.verify_chunk(&sha256_hex(&[b'a'; 65536]), CHUNK1).unwrap();
        let other = canonical_trailer([("x-amz-checksum-crc32", "AAAAAA==")]);
        assert!(v2.verify_trailer(&other, &sig).is_err());
    }

    #[test]
    fn canonical_trailer_skips_the_signature_header_and_keeps_order() {
        let canonical = canonical_trailer([
            ("x-amz-checksum-sha256", "abc"),
            ("X-Amz-Trailer-Signature", "deadbeef"),
            ("x-amz-checksum-crc32", "def"),
        ]);
        assert_eq!(canonical, b"x-amz-checksum-sha256:abc\nx-amz-checksum-crc32:def\n");
    }
}
