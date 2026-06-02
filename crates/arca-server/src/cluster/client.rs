//! Inter-node cluster transport client (Phase 29 HA).
//!
//! Sibling of the Phase 28 [`crate::replicator::client::OutboundClient`], but it
//! targets the dedicated internal `/cluster/v1/*` endpoints (never the public S3
//! path) so blob ids, version ids, and sidecars travel verbatim. Requests are
//! signed with the shared cluster credential (fixed access key + `[cluster]
//! .secret`) and carry this node's id in the loop-prevention source header.
//!
//! Blob bodies are streamed with [`reqwest::Body::wrap_stream`] — no buffering
//! of whole objects in memory (the limitation Phase 28 accepted via its
//! `collect_stream`).
//!
//! The `send_*` / `fetch_blob` methods are consumed by the M3 write-path
//! decorators (`ClusterBlobStore` / `ClusterMetadataStore`).

use std::time::Duration;

use arca_auth::{sign_outbound_request, SignOutboundInput};
use arca_core::cluster::{
    ClusterManifest, ClusterManifestRequest, ClusterVersionDelete, ControlOp, ControlSnapshot,
    CLUSTER_ACCESS_KEY, CLUSTER_REGION, CLUSTER_SIDECAR_HEADER,
};
use arca_core::store::{ByteStream, SidecarMeta};
use arca_core::types::{BlobId, ObjectRecord};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use futures_util::TryStreamExt;
use reqwest::header::HeaderMap;
use reqwest::Body;

use crate::sigv4_http::{base_signed_headers, now_iso8601, push_signed_headers};

/// Errors from the cluster transport client.
#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("peer returned HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("network error: {0}")]
    Network(String),

    #[error("malformed peer URL: {0}")]
    BadUrl(String),

    #[error("serialization error: {0}")]
    Serde(String),
}

/// Signed transport to a single cluster peer's `/cluster/v1/*` endpoints.
/// Cheap to clone (shares the underlying `reqwest::Client` connection pool).
#[derive(Clone)]
pub struct ClusterClient {
    http: reqwest::Client,
    /// This node's id, sent as the loop-prevention source header.
    node_id: String,
    /// Shared cluster secret (the secret half of the cluster credential).
    secret: String,
}

impl ClusterClient {
    /// Builds a client signing as this node, with the given request timeout.
    pub fn new(
        node_id: impl Into<String>,
        secret: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, ClusterError> {
        // TECHDEBT(TD-015): peers may present self-signed certs under clustered
        // TLS; until the shared cluster CA is wired, accept invalid certs for
        // these inter-node requests. Authentication is provided by SigV4 + the
        // shared secret (signed headers), not by TLS — TLS is confidentiality
        // only here, so accepting the cert does not weaken request auth.
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .danger_accept_invalid_certs(true)
            .user_agent(concat!("arca-cluster/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| ClusterError::Network(e.to_string()))?;
        Ok(Self {
            http,
            node_id: node_id.into(),
            secret: secret.into(),
        })
    }

    /// Replicates a fully-formed object row verbatim to a peer
    /// (`POST /cluster/v1/object`). The peer applies it via
    /// `MetadataStore::apply_remote_object` (idempotent, LWW).
    pub async fn send_object(
        &self,
        endpoint: &str,
        record: &ObjectRecord,
    ) -> Result<(), ClusterError> {
        let body = serde_json::to_vec(record).map_err(|e| ClusterError::Serde(e.to_string()))?;
        self.post_json(endpoint, "/cluster/v1/object", body).await
    }

    /// Replicates a hard-delete of a single object version to a peer
    /// (`POST /cluster/v1/object/delete`). `version_id == "null"` targets the
    /// null-version row.
    pub async fn send_version_delete(
        &self,
        endpoint: &str,
        bucket: &str,
        key: &str,
        version_id: &str,
    ) -> Result<(), ClusterError> {
        let payload = ClusterVersionDelete {
            bucket: bucket.to_string(),
            key: key.to_string(),
            version_id: version_id.to_string(),
        };
        let body = serde_json::to_vec(&payload).map_err(|e| ClusterError::Serde(e.to_string()))?;
        self.post_json(endpoint, "/cluster/v1/object/delete", body)
            .await
    }

    /// Replicates a control-plane operation to a peer (`POST /cluster/v1/op`).
    /// The peer applies it idempotently to its local stores.
    pub async fn send_op(&self, endpoint: &str, op: &ControlOp) -> Result<(), ClusterError> {
        let body = serde_json::to_vec(op).map_err(|e| ClusterError::Serde(e.to_string()))?;
        self.post_json(endpoint, "/cluster/v1/op", body).await
    }

    /// Pulls a peer's changed-since object manifest (`POST /cluster/v1/manifest`):
    /// every row the peer wrote with `seq > since`, up to `limit`, plus the
    /// cursor to advance. The anti-entropy worker loops this per peer until the
    /// batch is short. A POST (not a query-string GET) keeps the request body
    /// the `UNSIGNED-PAYLOAD` the signing path already uses.
    pub async fn fetch_manifest(
        &self,
        endpoint: &str,
        since: u64,
        limit: u32,
    ) -> Result<ClusterManifest, ClusterError> {
        let req = ClusterManifestRequest { since, limit };
        let body = serde_json::to_vec(&req).map_err(|e| ClusterError::Serde(e.to_string()))?;
        let bytes = self
            .post_json_recv(endpoint, "/cluster/v1/manifest", body)
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| ClusterError::Serde(e.to_string()))
    }

    /// Pulls a peer's full control-plane snapshot (`GET
    /// /cluster/v1/control-snapshot`) for the reconcile pass to merge.
    pub async fn fetch_control_snapshot(
        &self,
        endpoint: &str,
    ) -> Result<ControlSnapshot, ClusterError> {
        let bytes = self
            .get_recv(endpoint, "/cluster/v1/control-snapshot")
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| ClusterError::Serde(e.to_string()))
    }

    /// Streams a blob's raw bytes + sidecar to a peer (`PUT /cluster/v1/blob/{id}`),
    /// stored verbatim under the same `blob_id`. For composite blobs (which have
    /// no physical file) pass an empty `body`; the peer writes only the sidecar.
    ///
    /// The sidecar travels as the signed [`CLUSTER_SIDECAR_HEADER`] (base64 JSON)
    /// so the body stays a pure byte stream and the wrapped DEK an encrypted
    /// sidecar carries cannot be tampered with in transit.
    pub async fn send_blob(
        &self,
        endpoint: &str,
        blob_id: &BlobId,
        sidecar: &SidecarMeta,
        body: ByteStream,
    ) -> Result<(), ClusterError> {
        let sidecar_json =
            serde_json::to_vec(sidecar).map_err(|e| ClusterError::Serde(e.to_string()))?;
        let sidecar_b64 = BASE64.encode(&sidecar_json);

        let path = format!("/cluster/v1/blob/{}", blob_id.0);
        let (url, host, uri_path) = cluster_target(endpoint, &path)?;
        let datetime = now_iso8601();

        // content-length is deliberately NOT signed: the body streams with
        // chunked transfer-encoding (unknown length), so signing a length we
        // cannot honor on the wire would break the signature.
        let mut headers = base_signed_headers(&host, &datetime, None, None, &self.node_id);
        headers.push((CLUSTER_SIDECAR_HEADER.to_string(), sidecar_b64));
        let auth = self.sign("PUT", &uri_path, &headers, &datetime);

        let mut hmap = HeaderMap::new();
        push_signed_headers(&mut hmap, &headers, &auth);

        let resp = self
            .http
            .put(&url)
            .headers(hmap)
            .body(Body::wrap_stream(body))
            .send()
            .await
            .map_err(|e| ClusterError::Network(e.to_string()))?;
        Self::check(resp).await
    }

    /// Fetches a blob's raw bytes + sidecar from a peer (`GET /cluster/v1/blob/{id}`),
    /// for read-repair. Returns the sidecar (decoded from the signed response
    /// header) and the streaming body (empty for composite blobs). A 404 (peer
    /// does not have the blob) surfaces as [`ClusterError::Http`] so the caller
    /// can try the next peer.
    pub async fn fetch_blob(
        &self,
        endpoint: &str,
        blob_id: &BlobId,
    ) -> Result<(SidecarMeta, ByteStream), ClusterError> {
        let path = format!("/cluster/v1/blob/{}", blob_id.0);
        let (url, host, uri_path) = cluster_target(endpoint, &path)?;
        let datetime = now_iso8601();
        let headers = base_signed_headers(&host, &datetime, None, None, &self.node_id);
        let auth = self.sign("GET", &uri_path, &headers, &datetime);

        let mut hmap = HeaderMap::new();
        push_signed_headers(&mut hmap, &headers, &auth);

        let resp = self
            .http
            .get(&url)
            .headers(hmap)
            .send()
            .await
            .map_err(|e| ClusterError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClusterError::Http { status, body });
        }

        // Decode the sidecar from the signed response header (owned before the
        // body is consumed into a stream).
        let sidecar_b64 = resp
            .headers()
            .get(CLUSTER_SIDECAR_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| ClusterError::Serde("missing sidecar header in response".to_string()))?;
        let sidecar_json = BASE64
            .decode(sidecar_b64)
            .map_err(|e| ClusterError::Serde(format!("sidecar base64: {e}")))?;
        let sidecar: SidecarMeta = serde_json::from_slice(&sidecar_json)
            .map_err(|e| ClusterError::Serde(format!("sidecar json: {e}")))?;

        let stream: ByteStream = Box::pin(
            resp.bytes_stream()
                .map_err(|e| std::io::Error::other(e)),
        );
        Ok((sidecar, stream))
    }

    /// Signs and sends a JSON body via POST to a fixed cluster path, returning
    /// the raw `reqwest::Response` for the caller to interpret.
    async fn send_post(
        &self,
        endpoint: &str,
        path: &str,
        body: Vec<u8>,
    ) -> Result<reqwest::Response, ClusterError> {
        let (url, host, uri_path) = cluster_target(endpoint, path)?;
        let datetime = now_iso8601();
        let headers = base_signed_headers(&host, &datetime, None, None, &self.node_id);
        let auth = self.sign("POST", &uri_path, &headers, &datetime);

        let mut hmap = HeaderMap::new();
        push_signed_headers(&mut hmap, &headers, &auth);

        self.http
            .post(&url)
            .headers(hmap)
            .body(Body::from(body))
            .send()
            .await
            .map_err(|e| ClusterError::Network(e.to_string()))
    }

    /// Signs and sends a JSON body via POST, discarding the response body
    /// (fire-and-forget ops: object/version-delete/control-plane fan-out).
    async fn post_json(
        &self,
        endpoint: &str,
        path: &str,
        body: Vec<u8>,
    ) -> Result<(), ClusterError> {
        let resp = self.send_post(endpoint, path, body).await?;
        Self::check(resp).await
    }

    /// Signs and sends a JSON body via POST, returning the response body bytes
    /// on success (request/response ops: the manifest pull).
    async fn post_json_recv(
        &self,
        endpoint: &str,
        path: &str,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, ClusterError> {
        let resp = self.send_post(endpoint, path, body).await?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClusterError::Http { status, body });
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ClusterError::Network(e.to_string()))
    }

    /// Signs and sends a GET to a fixed cluster path, returning the response
    /// body bytes on success (read pulls: the control snapshot).
    async fn get_recv(&self, endpoint: &str, path: &str) -> Result<Vec<u8>, ClusterError> {
        let (url, host, uri_path) = cluster_target(endpoint, path)?;
        let datetime = now_iso8601();
        let headers = base_signed_headers(&host, &datetime, None, None, &self.node_id);
        let auth = self.sign("GET", &uri_path, &headers, &datetime);

        let mut hmap = HeaderMap::new();
        push_signed_headers(&mut hmap, &headers, &auth);

        let resp = self
            .http
            .get(&url)
            .headers(hmap)
            .send()
            .await
            .map_err(|e| ClusterError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let body = resp.text().await.unwrap_or_default();
            return Err(ClusterError::Http { status, body });
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| ClusterError::Network(e.to_string()))
    }

    /// Computes the SigV4 `Authorization` header for the cluster credential.
    fn sign(
        &self,
        method: &str,
        uri_path: &str,
        headers: &[(String, String)],
        datetime: &str,
    ) -> String {
        sign_outbound_request(&SignOutboundInput {
            method,
            uri_path,
            query_string: "",
            headers,
            payload_hash: "UNSIGNED-PAYLOAD",
            access_key_id: CLUSTER_ACCESS_KEY,
            secret_access_key: &self.secret,
            region: CLUSTER_REGION,
            service: "s3",
            request_datetime: datetime,
        })
    }

    /// Maps a non-2xx response to a [`ClusterError::Http`].
    async fn check(resp: reqwest::Response) -> Result<(), ClusterError> {
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            Ok(())
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(ClusterError::Http { status, body })
        }
    }
}

/// Builds `(url, host, uri_path)` for a fixed cluster path against a peer base
/// URL (e.g. `https://10.0.0.2:9000`). Cluster paths are fixed ASCII (blob ids
/// are UUIDs), so no percent-encoding is needed — the signed `uri_path` is
/// byte-identical to what reqwest puts on the wire.
fn cluster_target(
    endpoint: &str,
    path: &str,
) -> Result<(String, String, String), ClusterError> {
    let parsed = reqwest::Url::parse(endpoint).map_err(|e| ClusterError::BadUrl(e.to_string()))?;
    let scheme = parsed.scheme();
    let host_segment = parsed
        .host_str()
        .ok_or_else(|| ClusterError::BadUrl("endpoint has no host".to_string()))?;
    let port_segment = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let host = format!("{host_segment}{port_segment}");
    let base_path = parsed.path().trim_end_matches('/');
    let uri_path = format!("{base_path}{path}");
    let url = format!("{scheme}://{host}{uri_path}");
    Ok((url, host, uri_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_target_builds_url_host_path() {
        let (url, host, path) =
            cluster_target("https://10.0.0.2:9000", "/cluster/v1/object").unwrap();
        assert_eq!(host, "10.0.0.2:9000");
        assert_eq!(path, "/cluster/v1/object");
        assert_eq!(url, "https://10.0.0.2:9000/cluster/v1/object");
    }

    #[test]
    fn cluster_target_blob_path() {
        let (url, host, path) = cluster_target(
            "http://arca-2:9000/",
            "/cluster/v1/blob/550e8400-e29b-41d4-a716-446655440000",
        )
        .unwrap();
        assert_eq!(host, "arca-2:9000");
        assert_eq!(path, "/cluster/v1/blob/550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            url,
            "http://arca-2:9000/cluster/v1/blob/550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[test]
    fn cluster_target_rejects_invalid() {
        assert!(cluster_target("not-a-url", "/cluster/v1/object").is_err());
    }

    /// The signature this client produces must verify under the SAME secret on
    /// the receiving side (what the `cluster_auth` middleware does). This pins
    /// the client↔server SigV4 agreement (region, service, signed headers).
    #[test]
    fn signature_verifies_with_shared_secret() {
        let client = ClusterClient::new("node-a", "supersecret", Duration::from_secs(5)).unwrap();
        let (_url, host, uri_path) =
            cluster_target("http://node-b:9000", "/cluster/v1/object").unwrap();
        let datetime = "20260531T120000Z".to_string();
        let headers = base_signed_headers(&host, &datetime, None, None, &client.node_id);
        let auth = client.sign("POST", &uri_path, &headers, &datetime);

        let parsed = arca_auth::parse_authorization(&auth).unwrap();
        assert_eq!(parsed.access_key_id, CLUSTER_ACCESS_KEY);
        assert_eq!(parsed.region, CLUSTER_REGION);

        let input = arca_auth::VerifyInput {
            method: "POST",
            uri_path: &uri_path,
            query_string: "",
            headers: &headers,
            payload_hash: "UNSIGNED-PAYLOAD",
            auth: &parsed,
            secret_access_key: "supersecret",
            request_datetime: &datetime,
        };
        assert!(arca_auth::verify_request(&input).is_ok());
    }

    /// A different secret must NOT verify — the shared secret is the only thing
    /// authorizing an inter-node request.
    #[test]
    fn signature_rejected_with_wrong_secret() {
        let client = ClusterClient::new("node-a", "supersecret", Duration::from_secs(5)).unwrap();
        let (_url, host, uri_path) =
            cluster_target("http://node-b:9000", "/cluster/v1/object").unwrap();
        let datetime = "20260531T120000Z".to_string();
        let headers = base_signed_headers(&host, &datetime, None, None, &client.node_id);
        let auth = client.sign("POST", &uri_path, &headers, &datetime);

        let parsed = arca_auth::parse_authorization(&auth).unwrap();
        let input = arca_auth::VerifyInput {
            method: "POST",
            uri_path: &uri_path,
            query_string: "",
            headers: &headers,
            payload_hash: "UNSIGNED-PAYLOAD",
            auth: &parsed,
            secret_access_key: "WRONG-secret",
            request_datetime: &datetime,
        };
        assert!(arca_auth::verify_request(&input).is_err());
    }

    /// The blob PUT signs the sidecar header, so it appears in SignedHeaders and
    /// verifies; a tampered sidecar value then breaks verification.
    #[test]
    fn blob_sidecar_header_is_signed() {
        let client = ClusterClient::new("node-a", "s3cr3t", Duration::from_secs(5)).unwrap();
        let (_url, host, uri_path) = cluster_target(
            "http://node-b:9000",
            "/cluster/v1/blob/550e8400-e29b-41d4-a716-446655440000",
        )
        .unwrap();
        let datetime = "20260531T120000Z".to_string();
        let mut headers = base_signed_headers(&host, &datetime, None, None, &client.node_id);
        headers.push((CLUSTER_SIDECAR_HEADER.to_string(), "eyJhIjoxfQ==".to_string()));
        let auth = client.sign("PUT", &uri_path, &headers, &datetime);

        let parsed = arca_auth::parse_authorization(&auth).unwrap();
        assert!(parsed
            .signed_headers
            .iter()
            .any(|h| h == CLUSTER_SIDECAR_HEADER));

        // Genuine headers verify.
        let ok = arca_auth::VerifyInput {
            method: "PUT",
            uri_path: &uri_path,
            query_string: "",
            headers: &headers,
            payload_hash: "UNSIGNED-PAYLOAD",
            auth: &parsed,
            secret_access_key: "s3cr3t",
            request_datetime: &datetime,
        };
        assert!(arca_auth::verify_request(&ok).is_ok());

        // Tampered sidecar value breaks the signature.
        let mut tampered = headers.clone();
        for (k, v) in tampered.iter_mut() {
            if k == CLUSTER_SIDECAR_HEADER {
                *v = "dGFtcGVyZWQ=".to_string();
            }
        }
        let bad = arca_auth::VerifyInput {
            method: "PUT",
            uri_path: &uri_path,
            query_string: "",
            headers: &tampered,
            payload_hash: "UNSIGNED-PAYLOAD",
            auth: &parsed,
            secret_access_key: "s3cr3t",
            request_datetime: &datetime,
        };
        assert!(arca_auth::verify_request(&bad).is_err());
    }
}
