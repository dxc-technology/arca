//! Admin API handlers for grant (policy) management.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use arca_core::policy;

use crate::handlers::admin::AdminError;
use crate::state::AppState;

// -- Response types --

#[derive(Serialize)]
struct GrantResponse {
    grant_id: String,
    name: String,
    description: String,
    document: serde_json::Value,
    created_at: String,
    updated_at: String,
}

#[derive(Serialize)]
struct GrantListItem {
    grant_id: String,
    name: String,
    description: String,
    created_at: String,
}

#[derive(Deserialize)]
pub struct CreateGrantRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub document: serde_json::Value,
}

#[derive(Deserialize)]
pub struct UpdateGrantRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub document: Option<serde_json::Value>,
}

// -- Handlers --

/// GET /admin/grants — list all grants.
pub async fn list_grants(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AdminError> {
    let grants = state
        .grants
        .list_grants()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let response: Vec<GrantListItem> = grants
        .into_iter()
        .map(|g| GrantListItem {
            grant_id: g.grant_id,
            name: g.name,
            description: g.description,
            created_at: g.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(response))
}

/// POST /admin/grants — create a new grant.
pub async fn create_grant(
    State(state): State<AppState>,
    Json(body): Json<CreateGrantRequest>,
) -> Result<impl IntoResponse, AdminError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AdminError::bad_request("Grant name must not be empty"));
    }

    // Parse and validate the policy document.
    let doc_str = serde_json::to_string(&body.document)
        .map_err(|e| AdminError::bad_request(format!("Invalid JSON: {e}")))?;
    let document = policy::parse_policy_document(&doc_str)
        .map_err(|e| AdminError::bad_request(e))?;

    let errors = policy::validate_policy_document(&document);
    if !errors.is_empty() {
        return Err(AdminError::bad_request(format!(
            "Policy validation errors: {}",
            errors.join("; ")
        )));
    }

    let now = chrono::Utc::now();
    let grant = arca_core::types::Grant {
        grant_id: uuid::Uuid::new_v4().to_string(),
        name,
        description: body.description,
        document: document.clone(),
        created_at: now,
        updated_at: now,
    };

    state
        .grants
        .put_grant(&grant)
        .await
        .map_err(|e| {
            if e.to_string().contains("UNIQUE constraint") {
                AdminError::conflict(format!("Grant name \"{}\" already exists", grant.name))
            } else {
                AdminError::internal(e.to_string())
            }
        })?;

    let response = GrantResponse {
        grant_id: grant.grant_id,
        name: grant.name,
        description: grant.description,
        document: serde_json::to_value(&grant.document).unwrap_or_default(),
        created_at: grant.created_at.to_rfc3339(),
        updated_at: grant.updated_at.to_rfc3339(),
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// GET /admin/grants/{grant_id} — get grant detail.
pub async fn get_grant(
    State(state): State<AppState>,
    Path(grant_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let grant = state
        .grants
        .get_grant(&grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Grant {grant_id} not found")))?;

    Ok(Json(GrantResponse {
        grant_id: grant.grant_id,
        name: grant.name,
        description: grant.description,
        document: serde_json::to_value(&grant.document).unwrap_or_default(),
        created_at: grant.created_at.to_rfc3339(),
        updated_at: grant.updated_at.to_rfc3339(),
    }))
}

/// PUT /admin/grants/{grant_id} — update grant.
pub async fn update_grant(
    State(state): State<AppState>,
    Path(grant_id): Path<String>,
    Json(body): Json<UpdateGrantRequest>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .grants
        .get_grant(&grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Grant {grant_id} not found")))?;

    let parsed_doc = if let Some(ref doc_value) = body.document {
        let doc_str = serde_json::to_string(doc_value)
            .map_err(|e| AdminError::bad_request(format!("Invalid JSON: {e}")))?;
        let doc = policy::parse_policy_document(&doc_str)
            .map_err(|e| AdminError::bad_request(e))?;
        let errors = policy::validate_policy_document(&doc);
        if !errors.is_empty() {
            return Err(AdminError::bad_request(format!(
                "Policy validation errors: {}",
                errors.join("; ")
            )));
        }
        Some(doc)
    } else {
        None
    };

    state
        .grants
        .update_grant(
            &grant_id,
            body.name.as_deref(),
            body.description.as_deref(),
            parsed_doc.as_ref(),
        )
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /admin/grants/{grant_id} — delete grant.
pub async fn delete_grant(
    State(state): State<AppState>,
    Path(grant_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let deleted = state
        .grants
        .delete_grant(&grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AdminError::not_found(format!("Grant {grant_id} not found")))
    }
}
