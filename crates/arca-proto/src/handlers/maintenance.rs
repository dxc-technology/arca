//! Admin API handlers for maintenance jobs (Phase 30).
//!
//! JSON over `/admin/maintenance/*`, SigV4-authenticated like the rest of the
//! admin API. One job runs at a time; the worker (in arca-server) processes it.

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use http::StatusCode;
use serde::Deserialize;

use arca_core::store::maintenance::{
    MaintenanceJob, MaintenanceJobMode, MaintenanceJobStatus, MaintenanceStore,
    DEFAULT_MAX_JOB_LOGS,
};

use crate::handlers::admin::AdminError;
use crate::state::AppState;

/// Job types the admin API will accept. Extended per milestone (M3: migrate-db;
/// M4: migrate-topology).
const KNOWN_JOB_TYPES: &[&str] = &["noop", "encrypt", "decrypt"];

/// Default page size for the job-history list.
const DEFAULT_HISTORY_LIMIT: u32 = 50;

#[derive(Deserialize)]
pub struct CreateJobRequest {
    #[serde(rename = "type")]
    pub job_type: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

fn default_mode() -> String {
    "live".to_string()
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<u32>,
}

fn store(state: &AppState) -> Result<&std::sync::Arc<dyn MaintenanceStore>, AdminError> {
    state
        .maintenance_store
        .as_ref()
        .ok_or_else(|| AdminError::unavailable("maintenance store not available"))
}

/// POST /admin/maintenance/jobs — start a new job (rejected if one is active).
pub async fn create_job(
    State(state): State<AppState>,
    Json(body): Json<CreateJobRequest>,
) -> Result<impl IntoResponse, AdminError> {
    let store = store(&state)?;

    let job_type = body.job_type.trim().to_string();
    if !KNOWN_JOB_TYPES.contains(&job_type.as_str()) {
        return Err(AdminError::bad_request(format!(
            "unknown job type: \"{job_type}\""
        )));
    }
    let mode = MaintenanceJobMode::parse(&body.mode)
        .ok_or_else(|| AdminError::bad_request("mode must be \"live\" or \"maintenance\""))?;
    let params = if body.params.is_null() {
        serde_json::json!({})
    } else {
        body.params
    };

    // Single-job lock: refuse a new job while one is pending/running/paused.
    if store
        .active_job()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .is_some()
    {
        return Err(AdminError::conflict(
            "a maintenance job is already active; cancel it before starting another",
        ));
    }

    let now = chrono::Utc::now();
    let job = MaintenanceJob {
        id: uuid::Uuid::new_v4().to_string(),
        job_type,
        status: MaintenanceJobStatus::Pending.as_db().to_string(),
        mode: mode.as_db().to_string(),
        params,
        total: 0,
        done: 0,
        rate: 0.0,
        last_error: None,
        created_at: now,
        updated_at: now,
        started_at: None,
        finished_at: None,
    };
    store
        .create_job(&job)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok((StatusCode::CREATED, Json(job)))
}

/// GET /admin/maintenance/jobs — active job + recent history.
pub async fn list_jobs(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<impl IntoResponse, AdminError> {
    let store = store(&state)?;
    let limit = q.limit.unwrap_or(DEFAULT_HISTORY_LIMIT).min(500);
    let active = store
        .active_job()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    let jobs = store
        .list_jobs(limit)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "active": active, "jobs": jobs })))
}

/// GET /admin/maintenance/jobs/{id} — one job with its logs.
pub async fn get_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let store = store(&state)?;
    let job = store
        .get_job(&id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("job \"{id}\" not found")))?;
    let logs = store
        .list_job_logs(&id, DEFAULT_MAX_JOB_LOGS)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({ "job": job, "logs": logs })))
}

/// POST /admin/maintenance/jobs/{id}/pause — pause a pending/running job.
pub async fn pause_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    transition(
        &state,
        &id,
        &[MaintenanceJobStatus::Pending, MaintenanceJobStatus::Running],
        MaintenanceJobStatus::Paused,
        "pause",
    )
    .await
}

/// POST /admin/maintenance/jobs/{id}/resume — resume a paused job.
pub async fn resume_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    transition(
        &state,
        &id,
        &[MaintenanceJobStatus::Paused],
        MaintenanceJobStatus::Running,
        "resume",
    )
    .await
}

/// DELETE /admin/maintenance/jobs/{id} — cancel a non-terminal job.
pub async fn cancel_job(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    transition(
        &state,
        &id,
        &[
            MaintenanceJobStatus::Pending,
            MaintenanceJobStatus::Running,
            MaintenanceJobStatus::Paused,
        ],
        MaintenanceJobStatus::Cancelled,
        "cancel",
    )
    .await
}

/// Shared guard: a transition is allowed only from an expected current state.
async fn transition(
    state: &AppState,
    id: &str,
    allowed_from: &[MaintenanceJobStatus],
    to: MaintenanceJobStatus,
    verb: &str,
) -> Result<axum::response::Response, AdminError> {
    let store = store(state)?;
    let job = store
        .get_job(id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("job \"{id}\" not found")))?;
    let current = MaintenanceJobStatus::parse(&job.status)
        .ok_or_else(|| AdminError::internal(format!("bad status: {}", job.status)))?;
    if !allowed_from.contains(&current) {
        return Err(AdminError::conflict(format!(
            "cannot {verb} a job in state \"{}\"",
            job.status
        )));
    }
    store
        .set_job_status(id, to, None)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    let _ = store
        .append_job_log(id, "info", &format!("job {verb}d by operator"), DEFAULT_MAX_JOB_LOGS)
        .await;
    let updated = store
        .get_job(id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(updated).into_response())
}
