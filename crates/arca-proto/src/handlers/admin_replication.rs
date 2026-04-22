//! Admin API handlers for replication — Phase 28.
//!
//! Exposes:
//! - `GET /admin/replication/journal` — list journal entries (filter, paginate).
//! - `DELETE /admin/replication/journal` — clear every entry (confirm token).
//! - `POST /admin/replication/credentials/:name` — upsert destination credentials.
//! - `DELETE /admin/replication/credentials/:name` — remove a credential.
//! - `POST /admin/replication/retry/:id` — bump a failed entry back to pending.
//! - `POST /admin/replication/test-destination` — signed HEAD on the
//!   destination bucket to check endpoint reachability + credentials.
//!
//! Credentials are stored in `server_config` under the key
//! `replication.credentials.<name>` as `access_key_id:secret_access_key`.

use arca_core::store::replication::JournalFilter;
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use super::admin::AdminError;
use crate::state::AppState;

const CREDENTIAL_PREFIX: &str = "replication.credentials.";

#[derive(Deserialize)]
pub struct ListJournalQuery {
    pub bucket: Option<String>,
    pub status: Option<String>,
    pub rule_id: Option<String>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}

impl ListJournalQuery {
    fn to_filter(&self) -> JournalFilter {
        JournalFilter {
            bucket: self.bucket.clone(),
            status: self.status.clone(),
            rule_id: self.rule_id.clone(),
            offset: self.offset.unwrap_or(0),
            limit: self.limit.unwrap_or(100),
        }
    }
}

/// GET /admin/replication/journal — list journal entries.
pub async fn list_journal(
    State(state): State<AppState>,
    Query(q): Query<ListJournalQuery>,
) -> Result<impl IntoResponse, AdminError> {
    let filter = q.to_filter();
    let entries = state
        .replication_store
        .list_journal(&filter)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    let total = state
        .replication_store
        .count_journal(&filter)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "entries": entries,
        "total": total,
    })))
}

#[derive(Deserialize)]
pub struct UpsertCredentialRequest {
    pub access_key_id: String,
    pub secret_access_key: String,
}

/// POST /admin/replication/credentials/:name — upsert destination credentials.
///
/// Stored as `access_key:secret` in `server_config`. Deliberately simple — the
/// replication worker reads the same pair and splits on the first colon.
pub async fn upsert_credential(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<UpsertCredentialRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if name.is_empty() || name.contains('/') || name.contains(' ') {
        return Err(AdminError::bad_request(
            "credential name must be a non-empty token (no slashes/spaces)",
        ));
    }
    if body.access_key_id.is_empty() || body.secret_access_key.is_empty() {
        return Err(AdminError::bad_request(
            "access_key_id and secret_access_key are both required",
        ));
    }
    if body.secret_access_key.contains(':') {
        return Err(AdminError::bad_request(
            "secret_access_key must not contain ':' (storage uses colon as separator)",
        ));
    }
    let key = format!("{CREDENTIAL_PREFIX}{name}");
    let value = format!("{}:{}", body.access_key_id, body.secret_access_key);
    state
        .server_config
        .set_server_config(&key, &value)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "name": name, "access_key_id": body.access_key_id })))
}

/// DELETE /admin/replication/credentials/:name — remove a destination credential.
pub async fn delete_credential(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let key = format!("{CREDENTIAL_PREFIX}{name}");
    state
        .server_config
        .delete_server_config(&key)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// POST /admin/replication/retry/:id — reset a failed/pending entry so the
/// worker picks it up on the next tick.
pub async fn retry_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .replication_store
        .update_status(&id, "pending", 0, None, chrono::Utc::now())
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(http::StatusCode::NO_CONTENT.into_response())
}

/// Request body for `POST /admin/replication/test-destination`.
///
/// Either provide `credential_ref` to use an already-stored credential, or
/// provide `access_key_id` + `secret_access_key` inline (for the "+ New
/// credential" flow in the console, before the credential has been saved).
#[derive(Debug, Deserialize)]
pub struct TestDestinationRequest {
    pub endpoint: String,
    pub bucket: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
}

fn default_region() -> String {
    "us-east-1".to_string()
}

/// POST /admin/replication/test-destination — signed HEAD on the destination
/// bucket using the supplied credentials. Returns a diagnostic payload with
/// `success` + human-readable `status` the console can surface directly.
///
/// This is the same sequence the replication worker runs for every delivery:
/// build an AWS SigV4 HEAD, include the `x-amz-arca-replication-source`
/// loop-prevention header, hit the endpoint. A 2xx or 3xx response means
/// endpoint is reachable, credentials are accepted, and the destination
/// bucket exists. A 404 means endpoint/creds are fine but the bucket needs
/// to be created. A 403 means the signature was rejected (wrong creds or
/// clock skew). Network errors mean the endpoint is unreachable.
pub async fn test_destination(
    State(state): State<AppState>,
    Json(body): Json<TestDestinationRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if body.endpoint.is_empty() {
        return Err(AdminError::bad_request("endpoint is required"));
    }
    if body.bucket.is_empty() {
        return Err(AdminError::bad_request("bucket is required"));
    }

    // Resolve credentials: either inline or from server_config.
    let (ak, sk) = match (&body.credential_ref, &body.access_key_id, &body.secret_access_key) {
        (Some(r), None, None) => {
            let key = format!("{CREDENTIAL_PREFIX}{r}");
            let value = state
                .server_config
                .get_server_config(&key)
                .await
                .map_err(|e| AdminError::internal(e.to_string()))?
                .ok_or_else(|| AdminError::bad_request(format!(
                    "credential ref '{}' not found",
                    r
                )))?;
            let (a, s) = value.split_once(':').ok_or_else(|| {
                AdminError::internal(format!(
                    "stored credential '{}' is malformed",
                    r
                ))
            })?;
            (a.to_string(), s.to_string())
        }
        (None, Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => {
            (a.clone(), s.clone())
        }
        _ => {
            return Err(AdminError::bad_request(
                "provide either credential_ref OR both access_key_id and secret_access_key",
            ));
        }
    };

    // Build the signed HEAD request. Bucket names obey S3 naming rules, so
    // no path encoding needed — but see replicator/client.rs for the same
    // encode-key-segment discipline we apply to object paths.
    let parsed = match reqwest::Url::parse(&body.endpoint) {
        Ok(u) => u,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "success": false,
                "status": "invalid endpoint URL",
                "error": e.to_string(),
            })));
        }
    };
    let scheme = parsed.scheme();
    let host_segment = match parsed.host_str() {
        Some(h) => h.to_string(),
        None => {
            return Ok(Json(serde_json::json!({
                "success": false,
                "status": "endpoint has no host",
                "error": body.endpoint,
            })));
        }
    };
    let port_segment = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let host = format!("{host_segment}{port_segment}");
    let base_path = parsed.path().trim_end_matches('/');
    let uri_path = format!("{base_path}/{}", body.bucket);
    let url = format!("{scheme}://{host}{uri_path}");

    let source_id = if state.replication_source_id.is_empty() {
        "arca".to_string()
    } else {
        state.replication_source_id.clone()
    };
    let datetime = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let headers = vec![
        ("host".to_string(), host.clone()),
        ("x-amz-date".to_string(), datetime.clone()),
        (
            "x-amz-content-sha256".to_string(),
            "UNSIGNED-PAYLOAD".to_string(),
        ),
        (
            arca_core::s3::replication::REPLICATION_SOURCE_HEADER.to_string(),
            source_id,
        ),
    ];

    let auth = arca_auth::sign_outbound_request(&arca_auth::SignOutboundInput {
        method: "HEAD",
        uri_path: &uri_path,
        query_string: "",
        headers: &headers,
        payload_hash: "UNSIGNED-PAYLOAD",
        access_key_id: &ak,
        secret_access_key: &sk,
        region: &body.region,
        service: "s3",
        request_datetime: &datetime,
    });

    let mut hmap = reqwest::header::HeaderMap::new();
    for (k, v) in &headers {
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

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "success": false,
                "status": "failed to create HTTP client",
                "error": e.to_string(),
            })));
        }
    };

    let resp = match client.head(&url).headers(hmap).send().await {
        Ok(r) => r,
        Err(e) => {
            return Ok(Json(serde_json::json!({
                "success": false,
                "status": "network error — endpoint unreachable",
                "error": e.to_string(),
            })));
        }
    };

    let status = resp.status();
    let code = status.as_u16();
    // Capture the `Server` header to identify the destination software
    // (Arca emits "Arca" on every response via request_id middleware;
    // MinIO sends "MinIO"; AWS sends "AmazonS3"). The console uses this
    // to decide whether the loop-prevention contract applies.
    let server_header = resp
        .headers()
        .get("server")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let is_arca = server_header.as_deref().map(|s| s.starts_with("Arca")).unwrap_or(false);

    // 2xx: bucket exists + creds work.
    // 3xx: region redirect — creds likely fine, region may be wrong.
    // 404: endpoint + creds fine, bucket doesn't exist.
    // 403: signature rejected.
    let (ok, human) = match code {
        200..=299 => (true, format!("{code} — endpoint reachable, credentials accepted, bucket exists")),
        300..=399 => (true, format!("{code} — endpoint reachable, credentials accepted (redirect: probably a region mismatch)")),
        404 => (true, format!("404 — endpoint + credentials OK, but bucket '{}' does not exist on the destination yet", body.bucket)),
        403 => (false, "403 — signature rejected. Check credentials and clock skew.".to_string()),
        _ => (false, format!("{code} {}", status.canonical_reason().unwrap_or(""))),
    };
    let body_preview = resp.text().await.unwrap_or_default();

    Ok(Json(serde_json::json!({
        "success": ok,
        "status": human,
        "http_status": code,
        "server": server_header,
        "is_arca": is_arca,
        "response_body": body_preview.chars().take(500).collect::<String>(),
    })))
}

/// Request body for `DELETE /admin/replication/journal`.
#[derive(Debug, Deserialize)]
pub struct ClearJournalRequest {
    pub confirm: String,
}

/// DELETE /admin/replication/journal — clear every entry regardless of status.
/// Requires an explicit confirm token to avoid accidental bulk deletion.
pub async fn clear_journal(
    State(state): State<AppState>,
    Json(body): Json<ClearJournalRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if body.confirm != "CLEAR JOURNAL" {
        return Err(AdminError::bad_request(
            "Confirmation required: send {\"confirm\": \"CLEAR JOURNAL\"}",
        ));
    }
    // `purge_all_older(future)` truncates the table — reusing the same
    // retention primitive the hourly retention worker already uses.
    let cutoff = chrono::Utc::now() + chrono::Duration::days(1);
    let deleted = state
        .replication_store
        .purge_all_older(cutoff)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}
