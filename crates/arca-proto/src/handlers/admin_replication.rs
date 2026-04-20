//! Admin API handlers for replication — Phase 28.
//!
//! Exposes:
//! - `GET /admin/replication/journal` — list journal entries (filter, paginate).
//! - `GET /admin/replication/journal/count` — match count for pagination.
//! - `POST /admin/replication/credentials/:name` — upsert destination credentials.
//! - `DELETE /admin/replication/credentials/:name` — remove a credential.
//! - `POST /admin/replication/retry/:id` — bump a failed entry back to pending.
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
