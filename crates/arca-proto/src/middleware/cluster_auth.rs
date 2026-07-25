//! SigV4 authentication for inter-node cluster endpoints (`/cluster/v1/*`).
//!
//! A slimmed sibling of [`super::admin_auth`]. Unlike the S3 / admin paths it
//! does NOT resolve a stored credential, user, or grants: every authenticated
//! peer shares one fixed cluster credential — access key
//! [`arca_core::cluster::CLUSTER_ACCESS_KEY`], secret `[cluster].secret` (held
//! in [`AppState::cluster_secret`]). On top of the signature check it enforces
//! loop prevention: the request must carry the sender's id in the
//! `x-amz-arca-replication-source` header, and that id must not be this node's
//! own (a node never applies its own replication back to itself).
//!
//! Only header auth is accepted (no presigned query auth — peers always sign
//! with the `Authorization` header).

use axum::extract::State;
use axum::response::Response;
use chrono::Utc;
use http::StatusCode;

use arca_auth::{parse_authorization, verify_request, VerifyInput};
use arca_core::cluster::CLUSTER_ACCESS_KEY;
use arca_core::s3::replication::REPLICATION_SOURCE_HEADER;

use crate::state::AppState;

use super::within_replay_window;

/// Request extension recorded by the TLS accept loop when the connection
/// presented a client certificate that validated against the cluster CA
/// (R4/H12). Presenting one is OPTIONAL at the TLS layer — S3 clients share
/// the same listener — so [`cluster_auth_middleware`] enforces it here, on the
/// cluster routes only, whenever `[cluster.tls]` is configured.
#[derive(Clone, Copy, Debug)]
pub struct ClusterPeerCertVerified;

/// Request extension: the cluster secret that verified THIS request — the
/// current one or, during a rotation, `secret_previous` (decision H8). The
/// ping handler MACs its challenge nonce with this value: the prober signed
/// the request with the same secret and verifies the response against it, so
/// no rotation state produces a spurious failed-challenge verdict.
#[derive(Clone)]
pub struct MatchedClusterSecret(pub std::sync::Arc<str>);

/// Axum middleware: verify the shared cluster signature + loop prevention.
///
/// Returns a JSON error response on any failure; on success forwards the
/// request unchanged (cluster handlers need no identity in extensions).
pub async fn cluster_auth_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // The node must be part of a cluster and hold the shared secret.
    let self_node_id = match &state.cluster {
        Some(c) => c.node_id().to_string(),
        None => {
            return json_error(
                StatusCode::NOT_FOUND,
                "NotFound",
                "node is not part of a cluster",
            )
        }
    };
    let secret = match &state.cluster_secret {
        Some(s) if !s.is_empty() => s.clone(),
        _ => {
            return json_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "cluster secret is not configured",
            )
        }
    };

    // R4 (H12): when inter-node mTLS is configured, the TLS layer has already
    // validated any PRESENTED client certificate against the cluster CA — but
    // presenting one is optional there (S3 clients share the listener), so the
    // requirement is enforced here, on the cluster routes only.
    if state.cluster_mtls
        && request
            .extensions()
            .get::<ClusterPeerCertVerified>()
            .is_none()
    {
        return json_error(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "client certificate signed by the cluster CA is required on cluster endpoints",
        );
    }

    // Verify against the ORIGINAL URI (NormalizeLayer strips trailing slashes
    // after saving it), exactly as admin_auth does.
    let original_uri = request
        .extensions()
        .get::<crate::middleware::normalize::OriginalUri>()
        .map(|u| u.0.clone())
        .unwrap_or_else(|| request.uri().clone());
    let uri_path = original_uri.path().to_string();
    let query_string = original_uri.query().unwrap_or("").to_string();
    let method = request.method().as_str().to_string();

    // Collect headers; synthesize host from the HTTP/2 :authority when absent.
    let mut headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_string(), crate::header_value_to_string(value)))
        .collect();
    if !headers.iter().any(|(n, _)| n == "host") {
        if let Some(authority) = request.uri().authority() {
            headers.push(("host".to_string(), authority.as_str().to_string()));
        }
    }

    // Loop prevention: the sender's node id must be present and not our own.
    let source = headers
        .iter()
        .find(|(n, _)| n == REPLICATION_SOURCE_HEADER)
        .map(|(_, v)| v.clone());
    match source {
        Some(s) if s == self_node_id => {
            return json_error(
                StatusCode::CONFLICT,
                "LoopDetected",
                "request originates from this node",
            );
        }
        Some(s) if !s.is_empty() => {}
        _ => {
            return json_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "missing cluster source header",
            );
        }
    }

    // Parse + verify the SigV4 Authorization header.
    let auth_header = match request
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        Some(h) => h.to_string(),
        None => {
            return json_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                "missing Authorization header",
            )
        }
    };

    let parsed = match parse_authorization(&auth_header) {
        Ok(a) => a,
        Err(_) => {
            return json_error(
                StatusCode::FORBIDDEN,
                "SignatureDoesNotMatch",
                "The request signature we calculated does not match the signature you provided.",
            )
        }
    };

    // Only the fixed cluster credential is accepted on these endpoints.
    if parsed.access_key_id != CLUSTER_ACCESS_KEY {
        return json_error(
            StatusCode::FORBIDDEN,
            "InvalidAccessKeyId",
            "The cluster access key you provided is not recognized.",
        );
    }

    let request_datetime = request
        .headers()
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if request_datetime.is_empty() {
        return json_error(StatusCode::FORBIDDEN, "AccessDenied", "missing x-amz-date");
    }

    // §3.1 anti-replay: reject signed requests whose timestamp is outside the
    // window. The signature covers x-amz-date, so an attacker cannot refresh
    // the timestamp of a captured request without breaking it.
    if !within_replay_window(&request_datetime, Utc::now()) {
        return json_error(
            StatusCode::FORBIDDEN,
            "AccessDenied",
            "request time too far from server time (anti-replay window)",
        );
    }

    let payload_hash = request
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD")
        .to_string();

    // H8 dual-secret: try the current secret first, then (during a rotation)
    // the previous one. Each attempt is a full constant-time verification;
    // outbound signing always uses the current secret, so accepting the
    // previous one INBOUND is what lets a rolling restart onto a new secret
    // keep replication flowing in both directions.
    let mut matched: Option<&str> = None;
    for candidate in std::iter::once(secret.as_str()).chain(
        state
            .cluster_secret_previous
            .as_deref()
            .filter(|p| !p.is_empty()),
    ) {
        let input = VerifyInput {
            method: &method,
            uri_path: &uri_path,
            query_string: &query_string,
            headers: &headers,
            payload_hash: &payload_hash,
            auth: &parsed,
            secret_access_key: candidate,
            request_datetime: &request_datetime,
        };
        if verify_request(&input).is_ok() {
            matched = Some(candidate);
            break;
        }
    }
    let Some(matched) = matched else {
        return json_error(
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
        );
    };

    // Let the ping handler MAC its challenge with the secret that actually
    // verified this request (H8 — see [`MatchedClusterSecret`]).
    let mut request = request;
    request
        .extensions_mut()
        .insert(MatchedClusterSecret(std::sync::Arc::from(matched)));

    next.run(request).await
}

/// Builds a JSON error response (cluster endpoints speak JSON, not S3 XML).
fn json_error(status: StatusCode, error: &str, message: &str) -> Response {
    let body = serde_json::json!({
        "error": error,
        "message": message,
    });
    Response::builder()
        .status(status)
        .header("Content-Type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("build JSON error response")
}
