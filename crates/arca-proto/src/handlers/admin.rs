//! Admin API handlers.
//!
//! JSON-based administration endpoints under `/admin/*`.

use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::Json;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

// -- Error type --

/// Admin API error, serialized as JSON.
pub struct AdminError {
    status: StatusCode,
    error: &'static str,
    message: String,
}

impl AdminError {
    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: "InternalError",
            message: msg.into(),
        }
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            error: "NotFound",
            message: msg.into(),
        }
    }

    fn conflict(msg: impl Into<String>) -> Self {
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

#[derive(Serialize)]
struct InfoResponse {
    version: String,
    uptime_seconds: u64,
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
pub async fn health() -> impl IntoResponse {
    Json(HealthResponse { status: "ok" })
}

/// GET /admin/info — server version and uptime.
pub async fn info(State(state): State<AppState>) -> impl IntoResponse {
    Json(InfoResponse {
        version: state.version.clone(),
        uptime_seconds: state.started_at.elapsed().as_secs(),
    })
}

/// GET /admin/stats — aggregate storage statistics.
pub async fn stats(State(state): State<AppState>) -> Result<impl IntoResponse, AdminError> {
    let stats = state
        .metadata
        .get_stats()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    Ok(Json(stats))
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
    Json(body): Json<CreateCredentialRequest>,
) -> Result<impl IntoResponse, AdminError> {
    let cred = arca_core::credential::generate_credential(&body.description, body.admin);

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
