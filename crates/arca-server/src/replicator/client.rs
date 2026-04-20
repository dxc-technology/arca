//! Outbound S3 client used by the replication worker.
//!
//! Signs requests with AWS SigV4 via [`arca_auth::sign_outbound_request`] and
//! uses `reqwest` (rustls backend) to talk to any S3-compatible endpoint.
//! Deliberately minimal — only the operations Phase 28 replicates (PUT,
//! DELETE, PUT-tagging, HEAD).

use std::time::Duration;

use arca_auth::{sign_outbound_request, SignOutboundInput};
use arca_core::s3::replication::REPLICATION_SOURCE_HEADER;
use chrono::{DateTime, Utc};
use reqwest::header::HeaderMap;
use reqwest::Body;

/// All supported outbound operations.
#[derive(Debug, Clone)]
pub enum OutboundOp {
    /// PUT an object. `body` is already the object bytes (streaming is
    /// acceptable because `reqwest::Body` supports AsyncRead via `Body::wrap_stream`).
    Put {
        content_length: u64,
        content_type: Option<String>,
        /// Additional `x-amz-meta-*` headers to forward.
        user_metadata: Vec<(String, String)>,
    },
    /// DELETE an object (creates a delete marker on a versioned destination).
    Delete,
    /// PUT tagging — expects `body` to be the `<Tagging>` XML request body.
    PutTagging,
    /// HEAD — no body; used for conflict-check (destination `Last-Modified`).
    Head,
}

/// Errors from the outbound client. Wrapped into the journal entry's
/// `last_error` field by the worker.
#[derive(Debug, thiserror::Error)]
pub enum OutboundError {
    #[error("destination returned HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("network error: {0}")]
    Network(String),

    #[error("malformed destination URL: {0}")]
    BadUrl(String),
}

/// Result of a successful HEAD: destination's current `Last-Modified` (if any).
pub struct HeadResult {
    pub last_modified: Option<DateTime<Utc>>,
    pub exists: bool,
}

/// Outbound S3 client. Created once per journal batch.
#[derive(Clone)]
pub struct OutboundClient {
    http: reqwest::Client,
    source_id: String,
}

impl OutboundClient {
    pub fn new(source_id: impl Into<String>, timeout: Duration) -> Result<Self, OutboundError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("arca-replication/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| OutboundError::Network(e.to_string()))?;
        Ok(Self {
            http,
            source_id: source_id.into(),
        })
    }

    /// Perform a HEAD on the destination and return its `Last-Modified`.
    pub async fn head(
        &self,
        endpoint: &str,
        bucket: &str,
        key: &str,
        access_key_id: &str,
        secret_access_key: &str,
        region: &str,
    ) -> Result<HeadResult, OutboundError> {
        let (url, host, uri_path) = build_url(endpoint, bucket, key, None)?;
        let datetime = now_iso8601();

        let headers = base_signed_headers(&host, &datetime, None, None, &self.source_id);
        let auth = sign_outbound_request(&SignOutboundInput {
            method: "HEAD",
            uri_path: &uri_path,
            query_string: "",
            headers: &headers,
            payload_hash: "UNSIGNED-PAYLOAD",
            access_key_id,
            secret_access_key,
            region,
            service: "s3",
            request_datetime: &datetime,
        });

        let mut hmap = HeaderMap::new();
        push_signed_headers(&mut hmap, &headers, &auth);

        let resp = self
            .http
            .head(&url)
            .headers(hmap)
            .send()
            .await
            .map_err(|e| OutboundError::Network(e.to_string()))?;

        let status = resp.status().as_u16();
        if status == 404 {
            return Ok(HeadResult {
                last_modified: None,
                exists: false,
            });
        }
        if !(200..300).contains(&status) {
            return Err(OutboundError::Http {
                status,
                body: String::new(),
            });
        }
        let last_modified = resp
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| {
                DateTime::parse_from_rfc2822(s)
                    .ok()
                    .map(|d| d.with_timezone(&Utc))
            });
        Ok(HeadResult {
            last_modified,
            exists: true,
        })
    }

    /// Execute an outbound operation.
    ///
    /// For PUT/PutTagging, `body` is the request body. For DELETE/HEAD, `body`
    /// should be an empty vec.
    pub async fn execute(
        &self,
        op: &OutboundOp,
        endpoint: &str,
        bucket: &str,
        key: &str,
        body: Vec<u8>,
        access_key_id: &str,
        secret_access_key: &str,
        region: &str,
    ) -> Result<(), OutboundError> {
        let (method, query) = match op {
            OutboundOp::Put { .. } => ("PUT", None),
            OutboundOp::Delete => ("DELETE", None),
            OutboundOp::PutTagging => ("PUT", Some("tagging")),
            OutboundOp::Head => ("HEAD", None),
        };
        let (url, host, uri_path) = build_url(endpoint, bucket, key, query)?;
        let datetime = now_iso8601();

        let payload_hash = if body.is_empty() {
            "UNSIGNED-PAYLOAD"
        } else {
            // For small bodies (tags) we could SHA-256, but UNSIGNED-PAYLOAD
            // is always accepted and keeps the client streaming-friendly.
            "UNSIGNED-PAYLOAD"
        };

        let content_length = match op {
            OutboundOp::Put { content_length, .. } => Some(*content_length),
            OutboundOp::PutTagging => Some(body.len() as u64),
            _ => None,
        };
        let content_type = match op {
            OutboundOp::Put { content_type, .. } => content_type.clone(),
            OutboundOp::PutTagging => Some("application/xml".to_string()),
            _ => None,
        };
        let user_meta = match op {
            OutboundOp::Put { user_metadata, .. } => user_metadata.clone(),
            _ => Vec::new(),
        };

        let mut headers = base_signed_headers(
            &host,
            &datetime,
            content_length,
            content_type.as_deref(),
            &self.source_id,
        );
        for (k, v) in &user_meta {
            headers.push((k.to_lowercase(), v.clone()));
        }

        let query_str = query.unwrap_or("");
        let auth = sign_outbound_request(&SignOutboundInput {
            method,
            uri_path: &uri_path,
            query_string: query_str,
            headers: &headers,
            payload_hash,
            access_key_id,
            secret_access_key,
            region,
            service: "s3",
            request_datetime: &datetime,
        });

        let mut hmap = HeaderMap::new();
        push_signed_headers(&mut hmap, &headers, &auth);

        let req_url = if query_str.is_empty() {
            url
        } else {
            format!("{url}?{query_str}")
        };

        let mut builder = self.http.request(
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| OutboundError::Network(e.to_string()))?,
            &req_url,
        );
        builder = builder.headers(hmap);
        if !body.is_empty() {
            builder = builder.body(Body::from(body));
        }

        let resp = builder
            .send()
            .await
            .map_err(|e| OutboundError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(OutboundError::Http { status, body });
        }
        Ok(())
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn base_signed_headers(
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
        (
            REPLICATION_SOURCE_HEADER.to_string(),
            source_id.to_string(),
        ),
    ];
    if let Some(len) = content_length {
        v.push(("content-length".to_string(), len.to_string()));
    }
    if let Some(ct) = content_type {
        v.push(("content-type".to_string(), ct.to_string()));
    }
    v
}

fn push_signed_headers(hmap: &mut HeaderMap, headers: &[(String, String)], auth: &str) {
    for (k, v) in headers {
        if let (Ok(name), Ok(val)) = (
            reqwest::header::HeaderName::try_from(k.as_str()),
            reqwest::header::HeaderValue::try_from(v.as_str()),
        ) {
            hmap.insert(name, val);
        }
    }
    if let Ok(val) = reqwest::header::HeaderValue::try_from(auth) {
        hmap.insert(reqwest::header::AUTHORIZATION, val);
    }
}

/// Build the request URL + derive host + URI path.
fn build_url(
    endpoint: &str,
    bucket: &str,
    key: &str,
    _query: Option<&str>,
) -> Result<(String, String, String), OutboundError> {
    let parsed = reqwest::Url::parse(endpoint).map_err(|e| OutboundError::BadUrl(e.to_string()))?;
    let scheme = parsed.scheme();
    let host_segment = parsed
        .host_str()
        .ok_or_else(|| OutboundError::BadUrl("endpoint has no host".to_string()))?;
    let port_segment = parsed
        .port()
        .map(|p| format!(":{p}"))
        .unwrap_or_default();
    let host = format!("{host_segment}{port_segment}");

    // Path-style addressing (safe everywhere). Endpoint may contain a base path.
    let base_path = parsed.path().trim_end_matches('/');
    let uri_path = format!("{base_path}/{bucket}/{key}");
    let url = format!("{scheme}://{host}{uri_path}");
    Ok((url, host, uri_path))
}

fn now_iso8601() -> String {
    Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_path_style() {
        let (url, host, path) =
            build_url("https://replica.example.com", "mybucket", "my/key", None).unwrap();
        assert_eq!(host, "replica.example.com");
        assert_eq!(path, "/mybucket/my/key");
        assert_eq!(url, "https://replica.example.com/mybucket/my/key");
    }

    #[test]
    fn build_url_with_port() {
        let (url, host, path) =
            build_url("http://localhost:9001", "b", "k", None).unwrap();
        assert_eq!(host, "localhost:9001");
        assert_eq!(path, "/b/k");
        assert_eq!(url, "http://localhost:9001/b/k");
    }

    #[test]
    fn build_url_rejects_invalid() {
        assert!(build_url("not-a-url", "b", "k", None).is_err());
    }

    #[test]
    fn now_iso8601_format() {
        let s = now_iso8601();
        assert_eq!(s.len(), 16);
        assert!(s.ends_with('Z'));
    }
}
