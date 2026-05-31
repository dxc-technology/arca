//! Shared AWS SigV4 HTTP helpers for outbound, signed inter-node requests.
//!
//! Used by both the Phase 28 replication client ([`crate::replicator::client`])
//! and the Phase 29 cluster client ([`crate::cluster::client`]). Both sign
//! requests with [`arca_auth::sign_outbound_request`] and send them via
//! `reqwest`, and both carry the loop-prevention source header.

use arca_core::s3::replication::REPLICATION_SOURCE_HEADER;
use chrono::Utc;
use reqwest::header::HeaderMap;

/// Builds the base set of signed headers common to every outbound request:
/// `host`, `x-amz-date`, `x-amz-content-sha256` (always `UNSIGNED-PAYLOAD`),
/// and the loop-prevention source header. `content-length`/`content-type` are
/// appended when supplied.
pub fn base_signed_headers(
    host: &str,
    datetime: &str,
    content_length: Option<u64>,
    content_type: Option<&str>,
    source_id: &str,
) -> Vec<(String, String)> {
    let mut v = vec![
        ("host".to_string(), host.to_string()),
        ("x-amz-date".to_string(), datetime.to_string()),
        (
            "x-amz-content-sha256".to_string(),
            "UNSIGNED-PAYLOAD".to_string(),
        ),
        (REPLICATION_SOURCE_HEADER.to_string(), source_id.to_string()),
    ];
    if let Some(len) = content_length {
        v.push(("content-length".to_string(), len.to_string()));
    }
    if let Some(ct) = content_type {
        v.push(("content-type".to_string(), ct.to_string()));
    }
    v
}

/// Copies the signed headers and the computed `Authorization` value into a
/// `reqwest` [`HeaderMap`].
pub fn push_signed_headers(hmap: &mut HeaderMap, headers: &[(String, String)], auth: &str) {
    // CRITICAL: any header whose value is silently dropped here (because
    // reqwest rejects it) produces SignatureDoesNotMatch at the destination —
    // the signer already hashed that header's value into the canonical
    // request. Use `HeaderValue::from_bytes` which accepts non-ASCII UTF-8
    // bytes (e.g. "naïve" in x-amz-meta-* values); the signer hashed the
    // same bytes, so server and client agree. Header NAMES must still be
    // valid tokens (ASCII letters/digits/hyphens), which they always are for
    // the fixed set of headers we emit (host, x-amz-*, content-*) and for
    // x-amz-meta-* prefixed keys sanitized upstream by arca-proto.
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::try_from(k.as_str()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            hmap.insert(name, val);
        }
    }
    if let Ok(val) = reqwest::header::HeaderValue::from_bytes(auth.as_bytes()) {
        hmap.insert(reqwest::header::AUTHORIZATION, val);
    }
}

/// Current time formatted as the SigV4 compact ISO 8601 timestamp
/// (`YYYYMMDDTHHMMSSZ`).
pub fn now_iso8601() -> String {
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_signed_headers_preserves_non_ascii_metadata_values() {
        // Regression: `HeaderValue::try_from(&str)` would silently drop
        // non-ASCII values, so x-amz-meta-* headers that made it past the
        // signer disappeared on the wire → SignatureDoesNotMatch. Switching
        // to from_bytes preserves the bytes; the signer hashed the same
        // bytes, so signer and wire agree.
        let mut hmap = HeaderMap::new();
        push_signed_headers(
            &mut hmap,
            &[
                ("host".to_string(), "replica.example.com".to_string()),
                (
                    "x-amz-meta-description".to_string(),
                    "naïve café — résumé".to_string(),
                ),
            ],
            "AWS4-HMAC-SHA256 Credential=...",
        );
        let meta = hmap
            .get("x-amz-meta-description")
            .expect("metadata header preserved");
        assert_eq!(meta.as_bytes(), "naïve café — résumé".as_bytes());
    }

    #[test]
    fn now_iso8601_format() {
        let s = now_iso8601();
        assert_eq!(s.len(), 16);
        assert!(s.ends_with('Z'));
    }

    #[test]
    fn base_signed_headers_includes_required_headers() {
        let h = base_signed_headers(
            "node-b:9000",
            "20260531T120000Z",
            Some(42),
            Some("application/json"),
            "node-a-id",
        );
        let get = |name: &str| h.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        assert_eq!(get("host"), Some("node-b:9000"));
        assert_eq!(get("x-amz-date"), Some("20260531T120000Z"));
        assert_eq!(get("x-amz-content-sha256"), Some("UNSIGNED-PAYLOAD"));
        assert_eq!(get(REPLICATION_SOURCE_HEADER), Some("node-a-id"));
        assert_eq!(get("content-length"), Some("42"));
        assert_eq!(get("content-type"), Some("application/json"));
    }
}
