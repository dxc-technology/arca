//! Audit logging middleware.
//!
//! Captures operation name, status code, latency, identity, and bytes for
//! every request. Writes audit entries asynchronously via `tokio::spawn`
//! so it never blocks the response. Also feeds in-memory metrics.
//!
//! TECHDEBT(TD-009): Per-request inserts. Under heavy load, consider batching
//! via an mpsc channel with a dedicated writer task.

use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::extract::State;
use axum::middleware::Next;
use axum::response::Response;

use arca_core::store::audit::AuditEntry;

use crate::state::AppState;

/// Classify an S3/admin operation from the HTTP request.
///
/// Uses the same dispatch logic as the handlers: method + path + query params.
pub fn classify_operation(method: &str, path: &str, query: Option<&str>) -> &'static str {
    let query = query.unwrap_or("");

    // Admin API
    if path.starts_with("/admin/") {
        let admin_path = &path[7..]; // strip "/admin/"
        return match (method, admin_path) {
            ("GET", "health") => "Admin::Health",
            ("GET", "info") => "Admin::Info",
            ("GET", "stats") => "Admin::Stats",
            ("GET", "settings") => "Admin::ListSettings",
            ("GET", "metrics") => "Admin::Metrics",
            ("GET", "audit") => "Admin::ListAudit",
            ("GET", p) if p.starts_with("audit/") => "Admin::AuditStats",
            ("PUT", p) if p.starts_with("settings/") => "Admin::UpdateSetting",
            ("DELETE", p) if p.starts_with("settings/") => "Admin::DeleteSetting",
            ("GET", p) if p.starts_with("metrics/") => "Admin::MetricsHistory",
            ("POST", "credentials") => "Admin::CreateCredential",
            ("GET", "credentials") => "Admin::ListCredentials",
            ("PUT", p) if p.starts_with("credentials/") => "Admin::UpdateCredential",
            ("DELETE", p) if p.starts_with("credentials/") => "Admin::DeleteCredential",
            ("POST", "users") => "Admin::CreateUser",
            ("GET", "users") => "Admin::ListUsers",
            ("GET", "me") => "Admin::Me",
            ("GET", p) if p.starts_with("users/") => "Admin::GetUser",
            ("PUT", p) if p.starts_with("users/") => "Admin::UpdateUser",
            ("DELETE", p) if p.starts_with("users/") => "Admin::DeleteUser",
            ("POST", "teams") => "Admin::CreateTeam",
            ("GET", "teams") => "Admin::ListTeams",
            ("GET", p) if p.starts_with("teams/") => "Admin::GetTeam",
            ("PUT", p) if p.starts_with("teams/") => "Admin::UpdateTeam",
            ("DELETE", p) if p.starts_with("teams/") => "Admin::DeleteTeam",
            ("POST", "grants") => "Admin::CreateGrant",
            ("GET", "grants") => "Admin::ListGrants",
            ("GET", p) if p.starts_with("grants/") => "Admin::GetGrant",
            ("PUT", p) if p.starts_with("grants/") => "Admin::UpdateGrant",
            ("DELETE", p) if p.starts_with("grants/") => "Admin::DeleteGrant",
            ("POST", "archive") => "Admin::Archive",
            ("POST", "presign") => "Admin::Presign",
            _ => "Admin::Unknown",
        };
    }

    // S3 API — count path segments to determine scope
    let trimmed = path.trim_start_matches('/');
    let has_key = trimmed.contains('/');

    if trimmed.is_empty() {
        // Service level: GET / = ListBuckets
        return "ListBuckets";
    }

    if has_key {
        // Object-level: /{bucket}/{key...}
        // Check query params for multipart operations
        if query.contains("uploadId") && query.contains("partNumber") {
            return match method {
                "PUT" => "UploadPart",
                _ => "Unknown",
            };
        }
        if query.contains("uploadId") {
            return match method {
                "POST" => "CompleteMultipartUpload",
                "DELETE" => "AbortMultipartUpload",
                _ => "Unknown",
            };
        }
        if query.contains("uploads") {
            return "CreateMultipartUpload";
        }
        return match method {
            "GET" => "GetObject",
            "HEAD" => "HeadObject",
            "PUT" => {
                // Check for CopyObject (x-amz-copy-source)
                "PutObject"
            }
            "DELETE" => "DeleteObject",
            "POST" => "PostObject",
            _ => "Unknown",
        };
    }

    // Bucket-level: /{bucket}
    if query.contains("location") {
        return "GetBucketLocation";
    }
    if query.contains("uploads") {
        return "ListMultipartUploads";
    }
    if query.contains("versioning") {
        return match method {
            "GET" => "GetBucketVersioning",
            "PUT" => "PutBucketVersioning",
            _ => "Unknown",
        };
    }
    if query.contains("encryption") {
        return match method {
            "GET" => "GetBucketEncryption",
            "PUT" => "PutBucketEncryption",
            "DELETE" => "DeleteBucketEncryption",
            _ => "Unknown",
        };
    }
    if query.contains("delete") {
        return "DeleteObjects";
    }
    if query.contains("list-type") {
        return "ListObjectsV2";
    }

    match method {
        "GET" => "ListObjectsV1",
        "HEAD" => "HeadBucket",
        "PUT" => "CreateBucket",
        "DELETE" => "DeleteBucket",
        "POST" => "PostBucket",
        _ => "Unknown",
    }
}

/// Extract bucket and key from an S3 path like "/{bucket}/{key...}".
fn parse_bucket_key(path: &str) -> (Option<String>, Option<String>) {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return (None, None);
    }
    if let Some(idx) = trimmed.find('/') {
        let bucket = &trimmed[..idx];
        let key = &trimmed[idx + 1..];
        (
            Some(bucket.to_string()),
            if key.is_empty() { None } else { Some(key.to_string()) },
        )
    } else {
        (Some(trimmed.to_string()), None)
    }
}

/// Extract the access key ID from the Authorization header or query string.
/// Returns None if no SigV4 credentials are present (e.g., unauthenticated requests).
fn extract_access_key(request: &axum::extract::Request) -> Option<String> {
    // Try header auth: AWS4-HMAC-SHA256 Credential=KEY/DATE/REGION/SERVICE/aws4_request, ...
    if let Some(auth) = request.headers().get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some(cred_start) = auth.find("Credential=") {
            let rest = &auth[cred_start + 11..];
            if let Some(slash) = rest.find('/') {
                return Some(rest[..slash].to_string());
            }
        }
    }
    // Try query string auth: X-Amz-Credential=KEY/DATE/REGION/SERVICE/aws4_request
    if let Some(query) = request.uri().query() {
        for pair in query.split('&') {
            if let Some(val) = pair.strip_prefix("X-Amz-Credential=") {
                let decoded = urlencoding::decode(val).unwrap_or_default();
                if let Some(slash) = decoded.find('/') {
                    return Some(decoded[..slash].to_string());
                }
            }
        }
    }
    None
}

/// Audit middleware. Runs outermost (before auth) to capture all requests.
/// Extracts the access key from the Authorization header before auth runs,
/// then looks up the user asynchronously in the spawned audit task.
pub async fn audit_middleware(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let start = Instant::now();

    // Skip CORS preflight requests (OPTIONS) — they are infrastructure, not operations
    let is_options = request.method() == http::Method::OPTIONS;

    // Capture request info before passing to the handler
    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let query = request.uri().query().map(|q| q.to_string());

    // Extract access key from Authorization header BEFORE auth consumes the request
    let access_key_id = if is_options { None } else { extract_access_key(&request) };

    let source_ip = request
        .headers()
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            request
                .headers()
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
        })
        .map(|s| s.to_string());
    let user_agent = request
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let content_length: u64 = request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // Track active connections
    if let Some(ref metrics) = state.metrics_registry {
        metrics.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    // Run the inner service
    let response = next.run(request).await;

    // Decrement active connections
    if let Some(ref metrics) = state.metrics_registry {
        metrics.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    let duration = start.elapsed();
    let duration_ms = duration.as_millis() as u64;
    let status = response.status().as_u16();

    // Classify the operation
    let operation = classify_operation(&method, &path, query.as_deref());

    // Feed in-memory metrics (always, even if audit is disabled)
    if let Some(ref metrics) = state.metrics_registry {
        metrics.record_request(operation, status, duration_ms);
    }

    // Skip logging read-only monitoring/audit operations to avoid feedback loops
    // (e.g., viewing the audit log generates audit entries which appear in the audit log)
    let skip_audit = is_options || matches!(operation,
        "Admin::Health" | "Admin::Metrics" | "Admin::ListAudit" | "Admin::AuditStats"
        | "Admin::MetricsHistory" | "Admin::ListSettings"
    );

    // Write audit entry asynchronously (if enabled)
    if state.audit_enabled && !skip_audit {
        if let Some(ref audit_store) = state.audit_store {
            let (bucket, key) = if path.starts_with("/admin/") {
                (None, None)
            } else {
                parse_bucket_key(&path)
            };

            let audit = audit_store.clone();
            let credentials = state.credentials.clone();
            let request_id = response
                .headers()
                .get("x-amz-request-id")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            // TECHDEBT(TD-009): per-request insert + credential lookup
            tokio::spawn(async move {
                // Look up user_id from access key
                let user_id = if let Some(ref ak) = access_key_id {
                    match credentials.get_credential(ak).await {
                        Ok(Some(cred)) => Some(cred.user_id),
                        _ => None,
                    }
                } else {
                    None
                };

                let entry = AuditEntry {
                    id: 0,
                    timestamp: chrono::Utc::now(),
                    request_id,
                    operation: operation.to_string(),
                    bucket,
                    key,
                    version_id: None,
                    user_id,
                    access_key_id,
                    source_ip,
                    http_method: method,
                    http_status: status,
                    error_code: None,
                    bytes_sent: 0, // would need response body interception for accuracy
                    bytes_received: content_length,
                    duration_ms,
                    user_agent,
                };
                if let Err(e) = audit.insert_audit_entry(&entry).await {
                    tracing::warn!(error = %e, "Failed to write audit log entry");
                }
            });
        }
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_s3_operations() {
        assert_eq!(classify_operation("GET", "/", None), "ListBuckets");
        assert_eq!(classify_operation("PUT", "/my-bucket", None), "CreateBucket");
        assert_eq!(classify_operation("DELETE", "/my-bucket", None), "DeleteBucket");
        assert_eq!(classify_operation("HEAD", "/my-bucket", None), "HeadBucket");
        assert_eq!(classify_operation("GET", "/my-bucket", Some("location")), "GetBucketLocation");
        assert_eq!(classify_operation("GET", "/my-bucket", Some("list-type=2")), "ListObjectsV2");
        assert_eq!(classify_operation("GET", "/my-bucket", None), "ListObjectsV1");
        assert_eq!(classify_operation("PUT", "/bucket/key.txt", None), "PutObject");
        assert_eq!(classify_operation("GET", "/bucket/key.txt", None), "GetObject");
        assert_eq!(classify_operation("HEAD", "/bucket/key.txt", None), "HeadObject");
        assert_eq!(classify_operation("DELETE", "/bucket/key.txt", None), "DeleteObject");
        assert_eq!(classify_operation("POST", "/bucket/key.txt", Some("uploads")), "CreateMultipartUpload");
        assert_eq!(classify_operation("PUT", "/bucket/key.txt", Some("partNumber=1&uploadId=abc")), "UploadPart");
        assert_eq!(classify_operation("POST", "/bucket/key.txt", Some("uploadId=abc")), "CompleteMultipartUpload");
        assert_eq!(classify_operation("DELETE", "/bucket/key.txt", Some("uploadId=abc")), "AbortMultipartUpload");
        assert_eq!(classify_operation("POST", "/my-bucket", Some("delete")), "DeleteObjects");
        assert_eq!(classify_operation("GET", "/my-bucket", Some("versioning")), "GetBucketVersioning");
        assert_eq!(classify_operation("PUT", "/my-bucket", Some("versioning")), "PutBucketVersioning");
        assert_eq!(classify_operation("GET", "/my-bucket", Some("encryption")), "GetBucketEncryption");
        assert_eq!(classify_operation("PUT", "/my-bucket", Some("encryption")), "PutBucketEncryption");
        assert_eq!(classify_operation("DELETE", "/my-bucket", Some("encryption")), "DeleteBucketEncryption");
        assert_eq!(classify_operation("GET", "/my-bucket", Some("uploads")), "ListMultipartUploads");
    }

    #[test]
    fn classify_admin_operations() {
        assert_eq!(classify_operation("GET", "/admin/health", None), "Admin::Health");
        assert_eq!(classify_operation("GET", "/admin/info", None), "Admin::Info");
        assert_eq!(classify_operation("GET", "/admin/stats", None), "Admin::Stats");
        assert_eq!(classify_operation("GET", "/admin/settings", None), "Admin::ListSettings");
        assert_eq!(classify_operation("PUT", "/admin/settings/region", None), "Admin::UpdateSetting");
        assert_eq!(classify_operation("DELETE", "/admin/settings/region", None), "Admin::DeleteSetting");
        assert_eq!(classify_operation("POST", "/admin/users", None), "Admin::CreateUser");
        assert_eq!(classify_operation("GET", "/admin/users", None), "Admin::ListUsers");
        assert_eq!(classify_operation("POST", "/admin/teams", None), "Admin::CreateTeam");
        assert_eq!(classify_operation("POST", "/admin/grants", None), "Admin::CreateGrant");
        assert_eq!(classify_operation("POST", "/admin/presign", None), "Admin::Presign");
    }

    #[test]
    fn parse_bucket_key_from_path() {
        assert_eq!(parse_bucket_key("/"), (None, None));
        assert_eq!(parse_bucket_key("/my-bucket"), (Some("my-bucket".into()), None));
        assert_eq!(
            parse_bucket_key("/my-bucket/path/to/key.txt"),
            (Some("my-bucket".into()), Some("path/to/key.txt".into()))
        );
        assert_eq!(parse_bucket_key("/my-bucket/"), (Some("my-bucket".into()), None));
    }
}
