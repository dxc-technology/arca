//! Admin API handlers for team management.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::handlers::admin::AdminError;
use crate::state::AppState;

// -- Response types --

#[derive(Serialize)]
struct TeamResponse {
    team_id: String,
    name: String,
    description: String,
    created_at: String,
}

#[derive(Serialize)]
struct TeamDetailResponse {
    team_id: String,
    name: String,
    description: String,
    created_at: String,
    member_count: usize,
    grant_count: usize,
}

#[derive(Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Deserialize)]
pub struct UpdateTeamRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

// -- Handlers --

/// GET /admin/teams — list all teams.
pub async fn list_teams(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AdminError> {
    let teams = state
        .teams
        .list_teams()
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let mut response = Vec::new();
    for t in teams {
        let members = state.teams.list_members(&t.team_id).await.unwrap_or_default();
        let grants = state.grants.list_team_grants(&t.team_id).await.unwrap_or_default();
        response.push(serde_json::json!({
            "team_id": t.team_id,
            "name": t.name,
            "description": t.description,
            "created_at": t.created_at.to_rfc3339(),
            "member_count": members.len(),
            "grant_count": grants.len(),
        }));
    }

    Ok(Json(response))
}

/// POST /admin/teams — create a new team.
pub async fn create_team(
    State(state): State<AppState>,
    Json(body): Json<CreateTeamRequest>,
) -> Result<impl IntoResponse, AdminError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AdminError::bad_request("Team name must not be empty"));
    }

    let team = arca_core::types::Team {
        team_id: uuid::Uuid::new_v4().to_string(),
        name,
        description: body.description,
        created_at: chrono::Utc::now(),
    };

    state
        .teams
        .put_team(&team)
        .await
        .map_err(|e| {
            if e.to_string().contains("UNIQUE constraint") {
                AdminError::conflict(format!("Team name \"{}\" already exists", team.name))
            } else {
                AdminError::internal(e.to_string())
            }
        })?;

    let response = TeamResponse {
        team_id: team.team_id,
        name: team.name,
        description: team.description,
        created_at: team.created_at.to_rfc3339(),
    };

    Ok((StatusCode::CREATED, Json(response)))
}

/// GET /admin/teams/{team_id} — get team detail.
pub async fn get_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let team = state
        .teams
        .get_team(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Team {team_id} not found")))?;

    let members = state
        .teams
        .list_members(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let grants = state
        .grants
        .list_team_grants(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(TeamDetailResponse {
        team_id: team.team_id,
        name: team.name,
        description: team.description,
        created_at: team.created_at.to_rfc3339(),
        member_count: members.len(),
        grant_count: grants.len(),
    }))
}

/// PUT /admin/teams/{team_id} — update team.
pub async fn update_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    Json(body): Json<UpdateTeamRequest>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .teams
        .get_team(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Team {team_id} not found")))?;

    state
        .teams
        .update_team(
            &team_id,
            body.name.as_deref(),
            body.description.as_deref(),
        )
        .await
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                AdminError::conflict("Team name already exists")
            } else {
                AdminError::internal(e.to_string())
            }
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /admin/teams/{team_id} — delete team.
pub async fn delete_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let deleted = state
        .teams
        .delete_team(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AdminError::not_found(format!("Team {team_id} not found")))
    }
}

/// GET /admin/teams/{team_id}/members — list team members.
pub async fn list_members(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .teams
        .get_team(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Team {team_id} not found")))?;

    let members = state
        .teams
        .list_members(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let response: Vec<serde_json::Value> = members
        .into_iter()
        .map(|u| {
            serde_json::json!({
                "user_id": u.user_id,
                "username": u.username,
                "description": u.description,
                "is_root": u.is_root,
            })
        })
        .collect();

    Ok(Json(response))
}

/// PUT /admin/teams/{team_id}/members/{user_id} — add member.
pub async fn add_member(
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .teams
        .get_team(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Team {team_id} not found")))?;

    state
        .users
        .get_user(&user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("User {user_id} not found")))?;

    state
        .teams
        .add_member(&team_id, &user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /admin/teams/{team_id}/members/{user_id} — remove member.
pub async fn remove_member(
    State(state): State<AppState>,
    Path((team_id, user_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AdminError> {
    let removed = state
        .teams
        .remove_member(&team_id, &user_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    if removed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AdminError::not_found("Member not found in team"))
    }
}

/// GET /admin/teams/{team_id}/grants — list team grants.
pub async fn list_team_grants(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .teams
        .get_team(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Team {team_id} not found")))?;

    let grants = state
        .grants
        .list_team_grants(&team_id)
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

/// PUT /admin/teams/{team_id}/grants/{grant_id} — attach grant to team.
pub async fn attach_team_grant(
    State(state): State<AppState>,
    Path((team_id, grant_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AdminError> {
    state
        .teams
        .get_team(&team_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Team {team_id} not found")))?;

    state
        .grants
        .get_grant(&grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?
        .ok_or_else(|| AdminError::not_found(format!("Grant {grant_id} not found")))?;

    state
        .grants
        .attach_to_team(&team_id, &grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /admin/teams/{team_id}/grants/{grant_id} — detach grant from team.
pub async fn detach_team_grant(
    State(state): State<AppState>,
    Path((team_id, grant_id)): Path<(String, String)>,
) -> Result<impl IntoResponse, AdminError> {
    let detached = state
        .grants
        .detach_from_team(&team_id, &grant_id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    if detached {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AdminError::not_found("Grant not attached to team"))
    }
}
