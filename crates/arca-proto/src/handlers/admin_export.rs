//! Admin API handler for configuration export.
//!
//! `GET /admin/export` returns a JSON document containing the instance's
//! full configuration (settings, users, teams, grants, credentials, buckets,
//! bucket configs). Query parameters control which sections are included and
//! whether secret keys are exposed.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::handlers::admin::AdminError;
use crate::state::AppState;

/// Known bucket_config keys to export.
const BUCKET_CONFIG_KEYS: &[&str] = &[
    "versioning",
    "encryption_algorithm",
    "lifecycle_rules",
    "notification_configuration",
    "object_lock",
    "region",
];

/// All exportable section names.
const ALL_SECTIONS: &[&str] = &[
    "settings",
    "auth",
    "credentials",
    "buckets",
    "bucket_configs",
];

#[derive(Deserialize)]
pub struct ExportParams {
    /// Comma-separated list of sections (default: all).
    #[serde(default)]
    sections: Option<String>,
    /// Whether to include secret keys in plaintext (default: false).
    #[serde(default)]
    include_secrets: Option<bool>,
}

#[derive(Serialize)]
struct ExportMetadata {
    version: String,
    exported_at: String,
    sections: Vec<String>,
}

#[derive(Serialize)]
struct ExportedUser {
    user_id: String,
    username: String,
    description: String,
    is_root: bool,
    created_at: String,
}

#[derive(Serialize)]
struct ExportedTeam {
    team_id: String,
    name: String,
    description: String,
    created_at: String,
    members: Vec<String>,
}

#[derive(Serialize)]
struct ExportedGrant {
    grant_id: String,
    name: String,
    description: String,
    document: serde_json::Value,
    created_at: String,
    updated_at: String,
    users: Vec<String>,
    teams: Vec<String>,
}

#[derive(Serialize)]
struct ExportedCredential {
    access_key_id: String,
    secret_access_key: String,
    description: String,
    user_id: String,
    active: bool,
    created_at: String,
}

#[derive(Serialize)]
struct ExportedBucket {
    name: String,
    created_at: String,
    owner: String,
}

/// GET /admin/export — export instance configuration as JSON.
pub async fn export(
    State(state): State<AppState>,
    Query(params): Query<ExportParams>,
) -> Result<impl IntoResponse, AdminError> {
    let include_secrets = params.include_secrets.unwrap_or(false);

    // Parse requested sections.
    let sections: Vec<String> = if let Some(ref s) = params.sections {
        let requested: Vec<String> = s.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).collect();
        // Validate section names.
        for name in &requested {
            if !ALL_SECTIONS.contains(&name.as_str()) {
                return Err(AdminError::bad_request(format!("Unknown section: {name}")));
            }
        }
        requested
    } else {
        ALL_SECTIONS.iter().map(|s| s.to_string()).collect()
    };

    let has = |name: &str| sections.iter().any(|s| s == name);

    let mut doc = serde_json::Map::new();

    // Metadata
    doc.insert(
        "arca_export".to_string(),
        serde_json::to_value(ExportMetadata {
            version: state.version.clone(),
            exported_at: chrono::Utc::now().to_rfc3339(),
            sections: sections.clone(),
        })
        .unwrap(),
    );

    // Settings
    if has("settings") {
        let pairs = state
            .server_config
            .list_server_config()
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        let map: HashMap<String, String> = pairs.into_iter().collect();
        doc.insert("settings".to_string(), serde_json::to_value(map).unwrap());
    }

    // Auth (users, teams, grants)
    if has("auth") {
        // Users
        let users = state
            .users
            .list_users()
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        let exported_users: Vec<ExportedUser> = users
            .iter()
            .map(|u| ExportedUser {
                user_id: u.user_id.clone(),
                username: u.username.clone(),
                description: u.description.clone(),
                is_root: u.is_root,
                created_at: u.created_at.to_rfc3339(),
            })
            .collect();
        doc.insert("users".to_string(), serde_json::to_value(&exported_users).unwrap());

        // Teams (with members)
        let teams = state
            .teams
            .list_teams()
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        let mut exported_teams = Vec::new();
        for t in &teams {
            let members = state
                .teams
                .list_members(&t.team_id)
                .await
                .unwrap_or_default();
            exported_teams.push(ExportedTeam {
                team_id: t.team_id.clone(),
                name: t.name.clone(),
                description: t.description.clone(),
                created_at: t.created_at.to_rfc3339(),
                members: members.iter().map(|m| m.user_id.clone()).collect(),
            });
        }
        doc.insert("teams".to_string(), serde_json::to_value(&exported_teams).unwrap());

        // Grants (with user/team attachments)
        let grants = state
            .grants
            .list_grants()
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        let mut exported_grants = Vec::new();
        for g in &grants {
            // Collect user IDs attached to this grant.
            let mut grant_users = Vec::new();
            for u in &users {
                let user_grants = state
                    .grants
                    .list_user_grants(&u.user_id)
                    .await
                    .unwrap_or_default();
                if user_grants.iter().any(|ug| ug.grant_id == g.grant_id) {
                    grant_users.push(u.user_id.clone());
                }
            }
            // Collect team IDs attached to this grant.
            let mut grant_teams = Vec::new();
            for t in &teams {
                let team_grants = state
                    .grants
                    .list_team_grants(&t.team_id)
                    .await
                    .unwrap_or_default();
                if team_grants.iter().any(|tg| tg.grant_id == g.grant_id) {
                    grant_teams.push(t.team_id.clone());
                }
            }
            exported_grants.push(ExportedGrant {
                grant_id: g.grant_id.clone(),
                name: g.name.clone(),
                description: g.description.clone(),
                document: serde_json::to_value(&g.document).unwrap_or_default(),
                created_at: g.created_at.to_rfc3339(),
                updated_at: g.updated_at.to_rfc3339(),
                users: grant_users,
                teams: grant_teams,
            });
        }
        doc.insert("grants".to_string(), serde_json::to_value(&exported_grants).unwrap());
    }

    // Credentials
    if has("credentials") {
        let creds = state
            .credentials
            .list_credentials()
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        let exported_creds: Vec<ExportedCredential> = creds
            .into_iter()
            .map(|c| ExportedCredential {
                access_key_id: c.access_key_id,
                secret_access_key: if include_secrets {
                    c.secret_access_key
                } else {
                    "****".to_string()
                },
                description: c.description,
                user_id: c.user_id,
                active: c.active,
                created_at: c.created_at.to_rfc3339(),
            })
            .collect();
        doc.insert("credentials".to_string(), serde_json::to_value(&exported_creds).unwrap());
    }

    // Buckets
    if has("buckets") {
        let buckets = state
            .metadata
            .list_buckets()
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;
        let exported_buckets: Vec<ExportedBucket> = buckets
            .iter()
            .map(|b| ExportedBucket {
                name: b.name.clone(),
                created_at: b.created_at.to_rfc3339(),
                owner: b.owner.clone(),
            })
            .collect();
        doc.insert("buckets".to_string(), serde_json::to_value(&exported_buckets).unwrap());
    }

    // Bucket configs
    if has("bucket_configs") {
        let buckets = state
            .metadata
            .list_buckets()
            .await
            .map_err(|e| AdminError::internal(e.to_string()))?;

        let mut configs: HashMap<String, serde_json::Value> = HashMap::new();
        for bucket in &buckets {
            let mut bc = serde_json::Map::new();

            // Fetch each known config key.
            for &key in BUCKET_CONFIG_KEYS {
                if let Ok(Some(val)) = state.metadata.get_bucket_config(&bucket.name, key).await {
                    bc.insert(key.to_string(), serde_json::Value::String(val));
                }
            }

            // Fetch bucket tags.
            if let Ok(tags) = state.metadata.get_bucket_tags(&bucket.name).await {
                if !tags.is_empty() {
                    let tag_map: HashMap<String, String> = tags.into_iter().collect();
                    bc.insert("tags".to_string(), serde_json::to_value(tag_map).unwrap());
                }
            }

            if !bc.is_empty() {
                configs.insert(bucket.name.clone(), serde_json::Value::Object(bc));
            }
        }
        doc.insert("bucket_configs".to_string(), serde_json::to_value(configs).unwrap());
    }

    Ok(Json(serde_json::Value::Object(doc)))
}
