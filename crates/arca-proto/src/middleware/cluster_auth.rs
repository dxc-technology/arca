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
use http::StatusCode;

use arca_auth::{parse_authorization, verify_request, VerifyInput};
use arca_core::cluster::CLUSTER_ACCESS_KEY;
use arca_core::s3::replication::REPLICATION_SOURCE_HEADER;

use crate::state::AppState;

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

    let payload_hash = request
        .headers()
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("UNSIGNED-PAYLOAD")
        .to_string();

    let input = VerifyInput {
        method: &method,
        uri_path: &uri_path,
        query_string: &query_string,
        headers: &headers,
        payload_hash: &payload_hash,
        auth: &parsed,
        secret_access_key: &secret,
        request_datetime: &request_datetime,
    };

    if verify_request(&input).is_err() {
        return json_error(
            StatusCode::FORBIDDEN,
            "SignatureDoesNotMatch",
            "The request signature we calculated does not match the signature you provided.",
        );
    }

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
