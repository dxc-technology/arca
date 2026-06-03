//! Admin API handlers.
//!
//! JSON-based administration endpoints under `/admin/*`.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use std::collections::HashSet;
use std::ffi::CString;

use crate::handlers::admin_settings::effective_region;
use crate::middleware::admin_auth::AuthenticatedCredential;
use crate::middleware::identity::AuthenticatedIdentity;
use crate::state::AppState;

// -- Error type --

/// Admin API error, serialized as JSON.
pub struct AdminError {
    status: StatusCode,
    error: &'static str,
    message: String,
}

impl AdminError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: "InternalError",
            message: msg.into(),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: "NotFound",
            message: msg.into(),
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: "BadRequest",
            message: msg.into(),
        }
    }

    pub fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error: "Conflict",
            message: msg.into(),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": self.error,
            "message": self.message,
        });
        (self.status, Json(body)).into_response()
    }
}

// -- Response types --

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

/// Query for `GET /admin/health`. `?verbose=1` (or `true`) returns the detailed
/// cluster view instead of the bare liveness status.
#[derive(Deserialize)]
pub struct HealthQuery {
    #[serde(default)]
    verbose: Option<String>,
}

/// Detailed health body for `?verbose=1`: liveness plus the cluster topology and
/// write-quorum status (`cluster` is null on a single-node deployment).
#[derive(Serialize)]
struct VerboseHealthResponse {
    status: &'static str,
    draining: bool,
    cluster: Option<arca_core::cluster::ClusterSnapshot>,
}

#[derive(Serialize)]
struct InfoResponse {
    version: String,
    uptime_seconds: u64,
    tls_enabled: bool,
    encryption_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    kms_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    kms_endpoint: Option<String>,
    metadata_backend: String,
    /// Phase 28. True when this instance has a stable replication-source ID
    /// configured and the replication worker is wired.
    replication_enabled: bool,
}

#[derive(Serialize)]
struct CredentialResponse {
    access_key_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret_access_key: Option<String>,
    description: String,
    created_at: String,
    active: bool,
    admin: bool,
}

#[derive(Deserialize)]
pub struct CreateCredentialRequest {
    #[serde(default)]
    description: String,
    #[serde(default)]
    admin: bool,
}

// -- Handlers --

/// GET /admin/health — unauthenticated health check.
///
/// Default: 200 `{"status": "ok"}`, or 503 `{"status": "draining"}` during the
/// graceful shutdown drain window (so load balancers stop routing traffic).
/// This default shape is what the LB health check consumes and is unchanged.
///
/// `?verbose=1` returns a 200 with the cluster topology + write-quorum status
/// for operators / the console (`cluster` is null on single-node). It does not
/// 503 on drain so an inspector always gets the detail; the `status`/`draining`
/// fields convey the drain state.
pub async fn health(State(state): State<AppState>, Query(q): Query<HealthQuery>) -> Response {
    let draining = *state.draining.borrow();
    let verbose = matches!(q.verbose.as_deref(), Some("1") | Some("true") | Some(""));

    if verbose {
        let body = VerboseHealthResponse {
            status: if draining { "draining" } else { "ok" },
            draining,
            cluster: state.cluster.as_ref().map(|c| c.snapshot()),
        };
        return Json(body).into_response();
    }

    if draining {
        return Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header("Content-Type", "application/json")
            .body(Body::from(r#"{"status":"draining"}"#))
            .expect("build draining response");
    }
    Json(HealthResponse { status: "ok" }).into_response()
}

/// One node in the `GET /admin/cluster` topology view.
#[derive(Serialize)]
struct ClusterNodeView {
    node_id: String,
    /// Peer base URL; `null` for the local node (its own endpoint is not tracked).
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    alive: bool,
    /// Last successful health contact (RFC3339); `null` if never / local node.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_seen: Option<String>,
    /// True for the node serving this request.
    local: bool,
    /// Whether this node's cluster-critical config matches the local node's.
    config_ok: bool,
}

/// Console-friendly cluster topology for `GET /admin/cluster`. `enabled` is
/// false on a single-node (non-clustered) deployment, in which case the rest is
/// omitted.
#[derive(Serialize)]
struct ClusterAdminResponse {
    enabled: bool,
    /// Consistency mode: `"quorum"` (majority required to write) or
    /// `"available"` (always writable, best-effort replication).
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_id: Option<String>,
    /// Durable copies required to ACK a write (quorum mode only).
    #[serde(skip_serializing_if = "Option::is_none")]
    write_quorum: Option<u32>,
    /// Whether writes can currently be acknowledged.
    #[serde(skip_serializing_if = "Option::is_none")]
    has_write_quorum: Option<bool>,
    /// Live nodes (including self).
    #[serde(skip_serializing_if = "Option::is_none")]
    live_node_count: Option<usize>,
    /// Total known nodes (including self).
    #[serde(skip_serializing_if = "Option::is_none")]
    node_count: Option<usize>,
    /// False when any live peer's cluster-critical config differs from ours.
    #[serde(skip_serializing_if = "Option::is_none")]
    config_aligned: Option<bool>,
    /// Cluster-effective disk capacity (bytes): the MINIMUM total across live
    /// nodes. With full replication the smallest node bounds the cluster.
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_total_bytes: Option<u64>,
    /// Cluster-effective free space (bytes): the MINIMUM available across live
    /// nodes — what can still be written before some node fills.
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_available_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    nodes: Vec<ClusterNodeView>,
}

/// GET /admin/cluster — cluster topology + consistency mode for the console.
///
/// Returns `{"enabled": false}` on a single-node deployment, otherwise the mode
/// (quorum/available), write-quorum status, and the full node list (self first,
/// then peers) with per-node liveness — the data the console topology widget
/// renders with status colours/icons.
pub async fn cluster(State(state): State<AppState>) -> Response {
    let snap = match state.cluster.as_ref() {
        Some(c) => c.snapshot(),
        None => {
            return Json(ClusterAdminResponse {
                enabled: false,
                mode: None,
                node_id: None,
                write_quorum: None,
                has_write_quorum: None,
                live_node_count: None,
                node_count: None,
                config_aligned: None,
                disk_total_bytes: None,
                disk_available_bytes: None,
                nodes: Vec::new(),
            })
            .into_response()
        }
    };

    let mode = if snap.write_quorum.is_some() {
        "quorum"
    } else {
        "available"
    };

    // Self first (always live: it is serving this request), then peers.
    let mut nodes: Vec<ClusterNodeView> = Vec::with_capacity(snap.peers.len() + 1);
    nodes.push(ClusterNodeView {
        node_id: snap.node_id.clone(),
        endpoint: snap.local_endpoint.clone(),
        alive: true,
        last_seen: None,
        local: true,
        config_ok: true,
    });
    // A live peer whose config fingerprint differs is flagged; dead peers are
    // not judged (config_ok stays true).
    let config_aligned = snap.peers.iter().all(|p| !p.alive || p.config_ok);
    for p in &snap.peers {
        nodes.push(ClusterNodeView {
            node_id: p.node_id.clone(),
            endpoint: Some(p.endpoint.clone()),
            alive: p.alive,
            last_seen: p.last_seen.map(|t| t.to_rfc3339()),
            local: false,
            config_ok: p.config_ok,
        });
    }
    let node_count = nodes.len();

    // Cluster-effective disk = min over this node + alive peers.
    let (local_total, local_available) = aggregate_disk_stats(&state.data_dirs);
    let (disk_total_bytes, disk_available_bytes) = state
        .cluster
        .as_ref()
        .map(|c| c.min_disk(local_total, local_available))
        .unwrap_or((local_total, local_available));

    Json(ClusterAdminResponse {
        enabled: true,
        mode: Some(mode),
        node_id: Some(snap.node_id),
        write_quorum: snap.write_quorum,
        has_write_quorum: Some(snap.has_write_quorum),
        live_node_count: Some(snap.live_node_count),
        node_count: Some(node_count),
        config_aligned: Some(config_aligned),
        disk_total_bytes,
        disk_available_bytes,
        nodes,
    })
    .into_response()
}

/// GET /admin/metrics — Prometheus text exposition format (unauthenticated).
pub async fn prometheus_metrics(State(state): State<AppState>) -> Response {
    let (bucket_count, object_count, total_size_bytes) =
        match state.metadata.get_stats().await {
            Ok(stats) => (stats.bucket_count, stats.object_count, stats.total_size_bytes),
            Err(_) => (0, 0, 0),
        };

    let body = if let Some(ref registry) = state.metrics_registry {
        registry.render_prometheus(bucket_count, object_count, total_size_bytes)
    } else {
        String::new()
    };

    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
        .body(Body::from(body))
        .expect("build prometheus response")
}

/// GET /admin/info — server version and uptime.
pub async fn info(State(state): State<AppState>) -> impl IntoResponse {
    Json(InfoResponse {
        version: state.version.clone(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        tls_enabled: state.tls_enabled,
        encryption_enabled: state.encryption_enabled,
        kms_provider: state.kms_provider.clone(),
        kms_endpoint: state.kms_endpoint.clone(),
        metadata_backend: state.metadata_backend.clone(),
        replication_enabled: !state.replication_source_id.is_empty(),
    })
}

/// Filesystem stats via statvfs(2). Returns (device_id, total, available).
fn disk_stats(path: &std::path::Path) -> Option<(u64, u64, u64)> {
    let c_path = CString::new(path.to_str()?).ok()?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
            let total = stat.f_blocks as u64 * stat.f_frsize as u64;
            let available = stat.f_bavail as u64 * stat.f_frsize as u64;
            Some((stat.f_fsid as u64, total, available))
        } else {
            None
        }
    }
}

/// Aggregate filesystem stats across multiple data directories,
/// deduplicating by device ID (dirs on the same filesystem count once).
/// Returns `(total, available)` bytes. Shared with the cluster health handler
/// (peers gossip their disk stats) and the cluster write-space guard.
pub(crate) fn aggregate_disk_stats(dirs: &[std::path::PathBuf]) -> (Option<u64>, Option<u64>) {
    let mut seen_devices = HashSet::new();
    let mut total: u64 = 0;
    let mut available: u64 = 0;
    let mut any = false;

    for dir in dirs {
        if let Some((dev, t, a)) = disk_stats(dir) {
            if seen_devices.insert(dev) {
                total += t;
                available += a;
                any = true;
            }
        }
    }

    if any {
        (Some(total), Some(available))
    } else {
        (None, None)
    }
}

#[derive(Serialize)]
struct StatsResponse {
    bucket_count: u64,
    object_count: u64,
    total_size_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_total_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_available_bytes: Option<u64>,
}

/// GET /admin/stats — aggregate storage statistics.
pub async fn stats(State(state): State<AppState>) -> Result<impl IntoResponse, AdminError> {
    let stats = state
        .metadata
        .get_stats()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let (disk_total, disk_available) = aggregate_disk_stats(&state.data_dirs);

    Ok(Json(StatsResponse {
        bucket_count: stats.bucket_count,
        object_count: stats.object_count,
        total_size_bytes: stats.total_size_bytes,
        disk_total_bytes: disk_total,
        disk_available_bytes: disk_available,
    }))
}

/// GET /admin/credentials — list all credentials (secrets redacted).
pub async fn list_credentials(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AdminError> {
    let creds = state
        .credentials
        .list_credentials()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let response: Vec<CredentialResponse> = creds
        .into_iter()
        .map(|c| CredentialResponse {
            access_key_id: c.access_key_id,
            secret_access_key: None,
            description: c.description,
            created_at: c.created_at.to_rfc3339(),
            active: c.active,
            admin: c.admin,
        })
        .collect();

    Ok(Json(response))
}

/// POST /admin/credentials — create a new credential.
pub async fn create_credential(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<impl IntoResponse, AdminError> {
    let identity = request
        .extensions()
        .get::<AuthenticatedIdentity>()
        .ok_or_else(|| AdminError::internal("missing authenticated identity"))?
        .clone();

    let body_bytes = axum::body::to_bytes(request.into_body(), 65_536)
        .await
        .map_err(|e| AdminError::bad_request(e.to_string()))?;
    let body: CreateCredentialRequest =
        serde_json::from_slice(&body_bytes).map_err(|e| AdminError::bad_request(e.to_string()))?;

    let cred = arca_core::credential::generate_credential(
        &body.description,
        body.admin,
        &identity.user.user_id,
    );

    state
        .credentials
        .put_credential(&cred)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let response = CredentialResponse {
        access_key_id: cred.access_key_id,
        secret_access_key: Some(cred.secret_access_key),
        description: cred.description,
        created_at: cred.created_at.to_rfc3339(),
        active: cred.active,
        admin: cred.admin,
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// PUT /admin/credentials/{access_key_id} — update a credential.
pub async fn update_credential(
    State(state): State<AppState>,
    Path(access_key_id): Path<String>,
    Json(body): Json<UpdateCredentialRequest>,
) -> Result<impl IntoResponse, AdminError> {
    let cred = state
        .credentials
        .get_credential(&access_key_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Credential {access_key_id} not found")))?;

    if let Some(active) = body.active {
        // Prevent deactivating the last active credential or last active admin credential.
        if !active && cred.active {
            let all_creds = state
                .credentials
                .list_credentials()
                .await
                .map_err(|e| AdminError::internal(e.to_string()))?;

            if cred.admin {
                let active_admin_count = all_creds.iter().filter(|c| c.active && c.admin).count();
                if active_admin_count <= 1 {
                    return Err(AdminError::conflict(
                        "Cannot deactivate the last active admin credential",
                    ));
                }
            }

            let active_count = all_creds.iter().filter(|c| c.active).count();
            if active_count <= 1 {
                return Err(AdminError::conflict(
                    "Cannot deactivate the last active credential",
                ));
            }
        }
    }

    state
        .credentials
        .update_credential(
            &access_key_id,
            body.active,
            body.description.as_deref(),
        )
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UpdateCredentialRequest {
    #[serde(default)]
    pub active: Option<bool>,
    #[serde(default)]
    pub description: Option<String>,
}

/// DELETE /admin/credentials/{access_key_id} — delete a credential.
pub async fn delete_credential(
    State(state): State<AppState>,
    Path(access_key_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    // Prevent deleting the last active credential.
    let active_count = state
        .credentials
        .count_active_credentials()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    // Check if the target credential is active.
    let target = state
        .credentials
        .get_credential(&access_key_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    match &target {
        None => {
            return Err(AdminError::not_found(format!(
                "Credential {access_key_id} not found"
            )));
        }
        Some(cred) if cred.active && cred.admin => {
            // Prevent deleting the last admin credential (checked before active lockout
            // since it's the more specific constraint).
            let all_creds = state
                .credentials
                .list_credentials()
                .await
                .map_err(|e| AdminError::internal(e.to_string()))?;
            let admin_count = all_creds.iter().filter(|c| c.active && c.admin).count();
            if admin_count <= 1 {
                return Err(AdminError::conflict(
                    "Cannot delete the last admin credential",
                ));
            }
        }
        Some(cred) if cred.active && active_count <= 1 => {
            return Err(AdminError::conflict(
                "Cannot delete the last active credential",
            ));
        }
        _ => {}
    }

    state
        .credentials
        .delete_credential(&access_key_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

// -- Presigned URL generation --

/// Maximum presigned URL expiry: 7 days (604800 seconds), per AWS spec.
const MAX_PRESIGN_EXPIRES: u64 = 604_800;

#[derive(Deserialize)]
pub struct PresignRequest {
    bucket: String,
    key: String,
    #[serde(default = "default_method")]
    method: String,
    #[serde(default = "default_expires")]
    expires: u64,
    /// Optional base URL override (e.g. "https://arca.example.com:9443").
    /// When provided, the presigned URL uses this host/scheme instead of the
    /// request's Host header. Useful when the client connects through a
    /// different endpoint than the public-facing URL.
    endpoint: Option<String>,
}

fn default_method() -> String {
    "GET".to_string()
}

fn default_expires() -> u64 {
    3600
}

#[derive(Serialize)]
struct PresignResponse {
    url: String,
    expires_at: String,
    id: String,
}

/// Percent-encode a single URI path segment (RFC 3986).
fn percent_encode_segment(segment: &str) -> String {
    let mut result = String::with_capacity(segment.len() * 2);
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    result
}

/// POST /admin/presign — generate a presigned URL for an object.
pub async fn presign(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<impl IntoResponse, AdminError> {
    // Extract the authenticated credential from extensions.
    let auth_cred = request
        .extensions()
        .get::<AuthenticatedCredential>()
        .ok_or_else(|| AdminError::internal("missing authenticated credential"))?
        .clone();

    // Extract the Host header (fallback for URL construction).
    let request_host = request
        .headers()
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "localhost:9000".to_string());

    // Parse the body.
    let body_bytes = axum::body::to_bytes(request.into_body(), 65_536)
        .await
        .map_err(|e| AdminError::bad_request(e.to_string()))?;
    let body: PresignRequest =
        serde_json::from_slice(&body_bytes).map_err(|e| AdminError::bad_request(e.to_string()))?;

    // Validate method.
    let method = body.method.to_uppercase();
    if !["GET", "PUT", "HEAD", "DELETE"].contains(&method.as_str()) {
        return Err(AdminError::bad_request(format!(
            "Invalid method: {method}. Must be GET, PUT, HEAD, or DELETE."
        )));
    }

    // Validate expires.
    if body.expires == 0 || body.expires > MAX_PRESIGN_EXPIRES {
        return Err(AdminError::bad_request(format!(
            "expires must be between 1 and {MAX_PRESIGN_EXPIRES}"
        )));
    }

    // Resolve scheme and host from the optional endpoint override or fall back
    // to the request's Host header and TLS state.
    let (scheme, host) = if let Some(ref ep) = body.endpoint {
        let ep = ep.trim_end_matches('/');
        if let Some(rest) = ep.strip_prefix("https://") {
            ("https".to_string(), rest.to_string())
        } else if let Some(rest) = ep.strip_prefix("http://") {
            ("http".to_string(), rest.to_string())
        } else {
            // No scheme, assume same as TLS state.
            let s = if state.tls_enabled { "https" } else { "http" };
            (s.to_string(), ep.to_string())
        }
    } else {
        let s = if state.tls_enabled { "https" } else { "http" };
        (s.to_string(), request_host)
    };

    // Build the URL path with percent-encoded segments.
    let encoded_key = body
        .key
        .split('/')
        .map(|seg| percent_encode_segment(seg))
        .collect::<Vec<_>>()
        .join("/");
    let uri_path = format!("/{}/{}", body.bucket, encoded_key);

    // Generate the presigned URL.
    let now = chrono::Utc::now();
    let datetime = now.format("%Y%m%dT%H%M%SZ").to_string();
    let expires_at = now + chrono::Duration::seconds(body.expires as i64);

    // Look up the full credential (we need the secret key).
    let credential = state
        .credentials
        .get_credential(&auth_cred.0.access_key_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::internal("credential not found"))?;

    let region = effective_region(&state, Some(&body.bucket)).await;
    let query_string = arca_auth::generate_presigned_url(
        &method,
        &host,
        &uri_path,
        &[],
        &credential.access_key_id,
        &credential.secret_access_key,
        &region,
        body.expires,
        &datetime,
    );

    let url = format!("{scheme}://{host}{uri_path}?{query_string}");

    // Track the presigned URL (metadata only, not the URL itself).
    let record_id = uuid::Uuid::new_v4().to_string();
    if let Some(ref store) = state.presigned_url_store {
        let record = arca_core::store::presigned_url::PresignedUrlRecord {
            id: record_id.clone(),
            bucket: body.bucket.clone(),
            key: body.key.clone(),
            method: method.clone(),
            expires_seconds: body.expires,
            created_at: now,
            expires_at,
            access_key_id: credential.access_key_id.clone(),
        };
        if let Err(e) = store.insert_presigned_url(&record).await {
            tracing::warn!(error = %e, "failed to track presigned URL");
        }
    }

    Ok(Json(PresignResponse {
        url,
        expires_at: expires_at.to_rfc3339(),
        id: record_id,
    }))
}
