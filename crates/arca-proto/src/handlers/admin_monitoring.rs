//! Admin API handlers for audit log and metrics history.

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use http::StatusCode;
use serde::Deserialize;

use arca_core::store::audit::AuditFilter;

use crate::handlers::admin::AdminError;
use crate::state::AppState;

/// Query parameters for GET /admin/audit.
#[derive(Debug, Deserialize)]
pub struct AuditQueryParams {
    pub bucket: Option<String>,
    pub operation: Option<String>,
    pub user_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}

/// GET /admin/audit — list audit log entries with optional filters.
pub async fn list_audit(
    State(state): State<AppState>,
    Query(params): Query<AuditQueryParams>,
) -> Result<impl IntoResponse, AdminError> {
    let audit_store = state
        .audit_store
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Audit logging is not enabled"))?;

    let filter = AuditFilter {
        bucket: params.bucket,
        operation: params.operation,
        user_id: params.user_id,
        from: params
            .from
            .as_deref()
            .and_then(|s| s.parse().ok()),
        to: params
            .to
            .as_deref()
            .and_then(|s| s.parse().ok()),
        offset: params.offset.unwrap_or(0),
        limit: params.limit.unwrap_or(100).min(1000),
    };

    let entries = audit_store
        .list_audit_entries(&filter)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let total = audit_store
        .count_audit_entries(&filter)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "entries": entries,
        "total": total,
        "offset": filter.offset,
        "limit": filter.limit,
    })))
}

/// GET /admin/audit/stats — audit summary counts.
pub async fn audit_stats(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AdminError> {
    let audit_store = state
        .audit_store
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Audit logging is not enabled"))?;

    // Total count
    let total = audit_store
        .count_audit_entries(&AuditFilter::default())
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "total_entries": total,
    })))
}

/// Request body for DELETE /admin/audit.
#[derive(Debug, Deserialize)]
pub struct ClearAuditRequest {
    pub confirm: String,
}

/// DELETE /admin/audit — clear all audit log entries.
/// Requires confirmation: body must contain `{"confirm": "CLEAR AUDIT LOG"}`.
pub async fn clear_audit(
    State(state): State<AppState>,
    Json(body): Json<ClearAuditRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if body.confirm != "CLEAR AUDIT LOG" {
        return Err(AdminError::bad_request(
            "Confirmation required: send {\"confirm\": \"CLEAR AUDIT LOG\"}",
        ));
    }

    let audit_store = state
        .audit_store
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Audit logging is not enabled"))?;

    // Delete all entries by purging with a far-future cutoff
    let cutoff = chrono::Utc::now() + chrono::Duration::days(36500);
    let deleted = audit_store
        .purge_audit_entries(cutoff)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "deleted": deleted })),
    ))
}

/// Query parameters for GET /admin/metrics/history.
#[derive(Debug, Deserialize)]
pub struct MetricsHistoryParams {
    pub from: Option<String>,
    pub to: Option<String>,
    pub limit: Option<u32>,
}

/// GET /admin/metrics/history — historical metrics snapshots.
pub async fn metrics_history(
    State(state): State<AppState>,
    Query(params): Query<MetricsHistoryParams>,
) -> Result<impl IntoResponse, AdminError> {
    let metrics_store = state
        .metrics_store
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Metrics collection is not enabled"))?;

    let from = params.from.as_deref().and_then(|s| s.parse().ok());
    let to = params.to.as_deref().and_then(|s| s.parse().ok());
    let limit = params.limit.unwrap_or(500).min(5000);

    let snapshots = metrics_store
        .list_metrics_snapshots(from, to, limit)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "snapshots": snapshots,
        "count": snapshots.len(),
    })))
}
