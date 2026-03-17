//! Admin API handlers for user management.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::handlers::admin::AdminError;
use crate::middleware::identity::AuthenticatedIdentity;
use crate::state::AppState;

// -- Response types --

#[derive(Serialize)]
pub struct UserResponse {
    pub user_id: String,
    pub username: String,
    pub description: String,
    pub is_root: bool,
    pub created_at: String,
}

#[derive(Serialize)]
struct UserDetailResponse {
    user_id: String,
    username: String,
    description: String,
    is_root: bool,
    created_at: String,
    credential_count: usize,
    team_count: usize,
    grant_count: usize,
}

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub description: Option<String>,
}

// -- Handlers --

/// GET /admin/users — list all users.
pub async fn list_users(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AdminError> {
    let users = state
        .users
        .list_users()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let response: Vec<UserResponse> = users
        .into_iter()
        .map(|u| UserResponse {
            user_id: u.user_id,
            username: u.username,
            description: u.description,
            is_root: u.is_root,
            created_at: u.created_at.to_rfc3339(),
        })
        .collect();

    Ok(Json(response))
}

/// POST /admin/users — create a new user.
pub async fn create_user(
    State(state): State<AppState>,
    Json(body): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, AdminError> {
    let username = body.username.trim().to_string();
    if username.is_empty() {
        return Err(AdminError::bad_request("Username must not be empty"));
    }

    // Check for duplicate username.
    if state
        .users
        .get_user_by_username(&username)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .is_some()
    {
        return Err(AdminError::conflict(format!(
            "Username \"{username}\" already exists"
        )));
    }

    let user = arca_core::types::User {
        user_id: uuid::Uuid::new_v4().to_string(),
        username,
        description: body.description,
        is_root: false,
        created_at: chrono::Utc::now(),
    };

    state
        .users
        .put_user(&user)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let response = UserResponse {
        user_id: user.user_id,
        username: user.username,
        description: user.description,
        is_root: user.is_root,
        created_at: user.created_at.to_rfc3339(),
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// GET /admin/users/{user_id} — get user detail.
pub async fn get_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let user = state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    let creds = state
        .credentials
        .list_credentials()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    let cred_count = creds.iter().filter(|c| c.user_id == user_id).count();

    let teams = state
        .teams
        .list_user_teams(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let grants = state
        .grants
        .list_user_grants(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(UserDetailResponse {
        user_id: user.user_id,
        username: user.username,
        description: user.description,
        is_root: user.is_root,
        created_at: user.created_at.to_rfc3339(),
        credential_count: cred_count,
        team_count: teams.len(),
        grant_count: grants.len(),
    }))
}

/// PUT /admin/users/{user_id} — update user.
pub async fn update_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(body): Json<UpdateUserRequest>,
) -> Result<impl IntoResponse, AdminError> {
    // Check user exists.
    let user = state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    if user.is_root {
        return Err(AdminError::conflict("Cannot modify the root user"));
    }

    if let Some(desc) = body.description {
        state
            .users
            .update_user(&user_id, &desc)
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
    }

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /admin/users/{user_id} — delete user.
pub async fn delete_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let user = state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    if user.is_root {
        return Err(AdminError::conflict("Cannot delete the root user"));
    }

    // Check that user has no credentials (must be removed first).
    let creds = state
        .credentials
        .list_credentials()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;
    let has_creds = creds.iter().any(|c| c.user_id == user_id);
    if has_creds {
        return Err(AdminError::conflict(
            "Cannot delete user with active credentials. Remove credentials first.",
        ));
    }

    state
        .users
        .delete_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// GET /admin/users/{user_id}/credentials — list credentials for a user.
pub async fn list_user_credentials(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    // Verify user exists.
    state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    let all_creds = state
        .credentials
        .list_credentials()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let creds: Vec<serde_json::Value> = all_creds
        .into_iter()
        .filter(|c| c.user_id == user_id)
        .map(|c| {
            serde_json::json!({
                "access_key_id": c.access_key_id,
                "description": c.description,
                "created_at": c.created_at.to_rfc3339(),
                "active": c.active,
            })
        })
        .collect();

    Ok(Json(creds))
}

/// POST /admin/users/{user_id}/credentials — create credential for a user.
pub async fn create_user_credential(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(body): Json<CreateCredentialBody>,
) -> Result<impl IntoResponse, AdminError> {
    // Verify user exists.
    state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    let cred = arca_core::credential::generate_credential(
        &body.description,
        false, // admin flag is deprecated, new creds always false
        &user_id,
    );

    state
        .credentials
        .put_credential(&cred)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "access_key_id": cred.access_key_id,
            "secret_access_key": cred.secret_access_key,
            "description": cred.description,
            "created_at": cred.created_at.to_rfc3339(),
            "active": cred.active,
            "user_id": cred.user_id,
        })),
    ))
}

#[derive(Deserialize)]
pub struct CreateCredentialBody {
    #[serde(default)]
    description: String,
}

/// GET /admin/users/{user_id}/grants — list direct grants.
pub async fn list_user_grants(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    let grants = state
        .grants
        .list_user_grants(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let response: Vec<serde_json::Value> = grants
        .into_iter()
        .map(|g| {
            serde_json::json!({
                "grant_id": g.grant_id,
                "name": g.name,
                "description": g.description,
            })
        })
        .collect();

    Ok(Json(response))
}

/// PUT /admin/users/{user_id}/grants/{grant_id} — attach grant to user.
pub async fn attach_user_grant(
    State(state): State<AppState>,
    Path((user_id, grant_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    state
        .grants
        .get_grant(&grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Grant {grant_id} not found")))?;

    state
        .grants
        .attach_to_user(&user_id, &grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /admin/users/{user_id}/grants/{grant_id} — detach grant from user.
pub async fn detach_user_grant(
    State(state): State<AppState>,
    Path((user_id, grant_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AdminError> {
    let detached = state
        .grants
        .detach_from_user(&user_id, &grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    if detached {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AdminError::not_found("Grant not attached to user"))
    }
}

/// GET /admin/users/{user_id}/effective-grants — all effective grants.
pub async fn effective_user_grants(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    // Get direct grants
    let direct = state
        .grants
        .list_user_grants(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    // Get team grants
    let teams = state
        .teams
        .list_user_teams(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let mut team_grants = Vec::new();
    for team in &teams {
        let grants = state
            .grants
            .list_team_grants(&team.team_id)
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        for g in grants {
            team_grants.push(serde_json::json!({
                "grant_id": g.grant_id,
                "name": g.name,
                "description": g.description,
                "source": format!("team:{}", team.name),
            }));
        }
    }

    let direct_json: Vec<serde_json::Value> = direct
        .into_iter()
        .map(|g| {
            serde_json::json!({
                "grant_id": g.grant_id,
                "name": g.name,
                "description": g.description,
                "source": "direct",
            })
        })
        .collect();

    let mut all = direct_json;
    all.extend(team_grants);

    Ok(Json(all))
}

/// GET /admin/me — returns the authenticated user's identity.
pub async fn me(
    request: axum::extract::Request,
) -> Result<impl IntoResponse, AdminError> {
    let identity = request
        .extensions()
        .get::<AuthenticatedIdentity>()
        .ok_or_else(|| AdminError::internal("missing authenticated identity"))?
        .clone();

    // Collect all action patterns from effective policies.
    let mut actions: Vec<String> = Vec::new();
    if identity.is_root() {
        actions.push("*".to_string());
    } else {
        for doc in &identity.effective_policies {
            for stmt in &doc.statement {
                if matches!(stmt.effect, arca_core::policy::Effect::Allow) {
                    for a in &stmt.action {
                        if !actions.contains(a) {
                            actions.push(a.clone());
                        }
                    }
                }
            }
        }
    }

    Ok(Json(serde_json::json!({
        "user": {
            "user_id": identity.user.user_id,
            "username": identity.user.username,
            "is_root": identity.user.is_root,
        },
        "effective_actions": actions,
    })))
}
