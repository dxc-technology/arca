//! Admin API handler for configuration import.
//!
//! `POST /admin/import` accepts a JSON document (as produced by `/admin/export`)
//! and applies it to the running instance.  The `mode` query parameter controls
//! conflict resolution: `skip` (default), `overwrite`, or `dry_run`.

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::handlers::admin::AdminError;
use crate::state::AppState;

/// Known bucket_config keys (mirrors admin_export).
const BUCKET_CONFIG_KEYS: &[&str] = &[
    "versioning",
    "encryption_algorithm",
    "lifecycle_rules",
    "notification_configuration",
    "object_lock",
    "region",
];

#[derive(Deserialize)]
pub struct ImportParams {
    /// Conflict resolution mode: "skip" (default), "overwrite", or "dry_run".
    #[serde(default = "default_mode")]
    mode: String,
}

fn default_mode() -> String {
    "skip".to_string()
}

#[derive(Serialize, Default, Clone)]
struct SectionResult {
    #[serde(skip_serializing_if = "is_zero")]
    created: u32,
    #[serde(skip_serializing_if = "is_zero")]
    applied: u32,
    #[serde(skip_serializing_if = "is_zero")]
    skipped: u32,
    #[serde(skip_serializing_if = "is_zero")]
    errors: u32,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

#[derive(Serialize)]
struct ImportResponse {
    mode: String,
    results: HashMap<String, SectionResult>,
    errors: Vec<String>,
}

/// POST /admin/import — import configuration from JSON.
pub async fn import_config(
    State(state): State<AppState>,
    Query(params): Query<ImportParams>,
    Json(body): Json<serde_json::Value>,
) -> Result<impl IntoResponse, AdminError> {
    let mode = params.mode.as_str();
    if !["skip", "overwrite", "dry_run"].contains(&mode) {
        return Err(AdminError::bad_request(
            "mode must be one of: skip, overwrite, dry_run",
        ));
    }
    let dry_run = mode == "dry_run";
    let overwrite = mode == "overwrite";

    let mut results: HashMap<String, SectionResult> = HashMap::new();
    let mut errors: Vec<String> = Vec::new();

    // 1. Settings (no dependencies)
    if let Some(settings) = body.get("settings") {
        let mut sr = SectionResult::default();
        if let Some(obj) = settings.as_object() {
            for (key, value) in obj {
                let val_str = match value.as_str() {
                    Some(s) => s.to_string(),
                    None => value.to_string(),
                };
                let exists = state
                    .server_config
                    .get_server_config(key)
                    .await
                    .unwrap_or(None)
                    .is_some();
                if exists && !overwrite {
                    sr.skipped += 1;
                    continue;
                }
                if !dry_run {
                    match state.server_config.set_server_config(key, &val_str).await {
                        Ok(()) => sr.applied += 1,
                        Err(e) => {
                            sr.errors += 1;
                            errors.push(format!("settings/{key}: {e}"));
                        }
                    }
                } else {
                    sr.applied += 1;
                }
            }
        }
        results.insert("settings".to_string(), sr);
    }

    // 2. Users (no dependencies)
    if let Some(users_val) = body.get("users") {
        let mut sr = SectionResult::default();
        if let Some(arr) = users_val.as_array() {
            for item in arr {
                let user_id = item.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
                let username = item.get("username").and_then(|v| v.as_str()).unwrap_or("");
                if user_id.is_empty() || username.is_empty() {
                    sr.errors += 1;
                    errors.push("users: missing user_id or username".to_string());
                    continue;
                }

                let exists = state.users.get_user(user_id).await.unwrap_or(None).is_some();
                if exists && !overwrite {
                    sr.skipped += 1;
                    continue;
                }

                if !dry_run {
                    let description = item.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let is_root = item.get("is_root").and_then(|v| v.as_bool()).unwrap_or(false);
                    let created_at = item
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(chrono::Utc::now);

                    let user = arca_core::types::User {
                        user_id: user_id.to_string(),
                        username: username.to_string(),
                        description: description.to_string(),
                        is_root,
                        created_at,
                    };

                    if exists {
                        // Overwrite: update existing user.
                        match state
                            .users
                            .update_user(user_id, Some(username), Some(description))
                            .await
                        {
                            Ok(_) => sr.created += 1,
                            Err(e) => {
                                sr.errors += 1;
                                errors.push(format!("users/{user_id}: {e}"));
                            }
                        }
                    } else {
                        match state.users.put_user(&user).await {
                            Ok(()) => sr.created += 1,
                            Err(e) => {
                                sr.errors += 1;
                                errors.push(format!("users/{user_id}: {e}"));
                            }
                        }
                    }
                } else {
                    sr.created += 1;
                }
            }
        }
        results.insert("users".to_string(), sr);
    }

    // 3. Teams (depends on users for member lists)
    if let Some(teams_val) = body.get("teams") {
        let mut sr = SectionResult::default();
        if let Some(arr) = teams_val.as_array() {
            for item in arr {
                let team_id = item.get("team_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if team_id.is_empty() || name.is_empty() {
                    sr.errors += 1;
                    errors.push("teams: missing team_id or name".to_string());
                    continue;
                }

                let exists = state.teams.get_team(team_id).await.unwrap_or(None).is_some();
                if exists && !overwrite {
                    sr.skipped += 1;
                    continue;
                }

                if !dry_run {
                    let description = item.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let created_at = item
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(chrono::Utc::now);

                    if exists {
                        match state
                            .teams
                            .update_team(team_id, Some(name), Some(description))
                            .await
                        {
                            Ok(_) => sr.created += 1,
                            Err(e) => {
                                sr.errors += 1;
                                errors.push(format!("teams/{team_id}: {e}"));
                            }
                        }
                    } else {
                        let team = arca_core::types::Team {
                            team_id: team_id.to_string(),
                            name: name.to_string(),
                            description: description.to_string(),
                            created_at,
                        };
                        match state.teams.put_team(&team).await {
                            Ok(()) => sr.created += 1,
                            Err(e) => {
                                sr.errors += 1;
                                errors.push(format!("teams/{team_id}: {e}"));
                            }
                        }
                    }

                    // Add members.
                    if let Some(members) = item.get("members").and_then(|v| v.as_array()) {
                        for member in members {
                            if let Some(uid) = member.as_str() {
                                let _ = state.teams.add_member(team_id, uid).await;
                            }
                        }
                    }
                } else {
                    sr.created += 1;
                }
            }
        }
        results.insert("teams".to_string(), sr);
    }

    // 4. Grants (depends on users and teams for attachments)
    if let Some(grants_val) = body.get("grants") {
        let mut sr = SectionResult::default();
        if let Some(arr) = grants_val.as_array() {
            for item in arr {
                let grant_id = item.get("grant_id").and_then(|v| v.as_str()).unwrap_or("");
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if grant_id.is_empty() || name.is_empty() {
                    sr.errors += 1;
                    errors.push("grants: missing grant_id or name".to_string());
                    continue;
                }

                let exists = state.grants.get_grant(grant_id).await.unwrap_or(None).is_some();
                if exists && !overwrite {
                    sr.skipped += 1;
                    continue;
                }

                if !dry_run {
                    let description = item.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let doc_value = item.get("document").cloned().unwrap_or(serde_json::Value::Null);
                    let doc_str = serde_json::to_string(&doc_value)
                        .map_err(|e| AdminError::bad_request(format!("Invalid grant document: {e}")))?;
                    let document = arca_core::policy::parse_policy_document(&doc_str)
                        .map_err(|e| AdminError::bad_request(format!("Invalid policy in grant {name}: {e}")))?;

                    let created_at = item
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(chrono::Utc::now);
                    let updated_at = item
                        .get("updated_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(chrono::Utc::now);

                    if exists {
                        match state
                            .grants
                            .update_grant(grant_id, Some(name), Some(description), Some(&document))
                            .await
                        {
                            Ok(_) => sr.created += 1,
                            Err(e) => {
                                sr.errors += 1;
                                errors.push(format!("grants/{grant_id}: {e}"));
                            }
                        }
                    } else {
                        let grant = arca_core::types::Grant {
                            grant_id: grant_id.to_string(),
                            name: name.to_string(),
                            description: description.to_string(),
                            document,
                            created_at,
                            updated_at,
                        };
                        match state.grants.put_grant(&grant).await {
                            Ok(()) => sr.created += 1,
                            Err(e) => {
                                sr.errors += 1;
                                errors.push(format!("grants/{grant_id}: {e}"));
                            }
                        }
                    }

                    // Attach to users.
                    if let Some(user_ids) = item.get("users").and_then(|v| v.as_array()) {
                        for uid in user_ids {
                            if let Some(uid_str) = uid.as_str() {
                                let _ = state.grants.attach_to_user(uid_str, grant_id).await;
                            }
                        }
                    }

                    // Attach to teams.
                    if let Some(team_ids) = item.get("teams").and_then(|v| v.as_array()) {
                        for tid in team_ids {
                            if let Some(tid_str) = tid.as_str() {
                                let _ = state.grants.attach_to_team(tid_str, grant_id).await;
                            }
                        }
                    }
                } else {
                    sr.created += 1;
                }
            }
        }
        results.insert("grants".to_string(), sr);
    }

    // 5. Credentials (depends on users)
    if let Some(creds_val) = body.get("credentials") {
        let mut sr = SectionResult::default();
        if let Some(arr) = creds_val.as_array() {
            for item in arr {
                let access_key_id = item.get("access_key_id").and_then(|v| v.as_str()).unwrap_or("");
                let secret = item.get("secret_access_key").and_then(|v| v.as_str()).unwrap_or("");

                if access_key_id.is_empty() {
                    sr.errors += 1;
                    errors.push("credentials: missing access_key_id".to_string());
                    continue;
                }

                // Skip masked credentials.
                if secret == "****" {
                    sr.skipped += 1;
                    errors.push(format!("credentials/{access_key_id}: masked secret, skipped"));
                    continue;
                }

                let exists = state
                    .credentials
                    .get_credential(access_key_id)
                    .await
                    .unwrap_or(None)
                    .is_some();
                if exists && !overwrite {
                    sr.skipped += 1;
                    continue;
                }

                if !dry_run {
                    let description = item.get("description").and_then(|v| v.as_str()).unwrap_or("");
                    let user_id = item.get("user_id").and_then(|v| v.as_str()).unwrap_or("");
                    let active = item.get("active").and_then(|v| v.as_bool()).unwrap_or(true);
                    let created_at = item
                        .get("created_at")
                        .and_then(|v| v.as_str())
                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&chrono::Utc))
                        .unwrap_or_else(chrono::Utc::now);

                    let cred = arca_core::types::Credential {
                        access_key_id: access_key_id.to_string(),
                        secret_access_key: secret.to_string(),
                        description: description.to_string(),
                        user_id: user_id.to_string(),
                        active,
                        admin: false,
                        created_at,
                    };

                    // put_credential handles both insert and upsert.
                    match state.credentials.put_credential(&cred).await {
                        Ok(()) => sr.created += 1,
                        Err(e) => {
                            sr.errors += 1;
                            errors.push(format!("credentials/{access_key_id}: {e}"));
                        }
                    }
                } else {
                    sr.created += 1;
                }
            }
        }
        results.insert("credentials".to_string(), sr);
    }

    // 6. Buckets (no dependencies)
    if let Some(buckets_val) = body.get("buckets") {
        let mut sr = SectionResult::default();
        if let Some(arr) = buckets_val.as_array() {
            for item in arr {
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    sr.errors += 1;
                    errors.push("buckets: missing name".to_string());
                    continue;
                }

                let exists = state
                    .metadata
                    .head_bucket(name)
                    .await
                    .unwrap_or(None)
                    .is_some();
                if exists {
                    sr.skipped += 1;
                    continue;
                }

                if !dry_run {
                    match state.metadata.create_bucket(name).await {
                        Ok(()) => sr.created += 1,
                        Err(e) => {
                            sr.errors += 1;
                            errors.push(format!("buckets/{name}: {e}"));
                        }
                    }
                } else {
                    sr.created += 1;
                }
            }
        }
        results.insert("buckets".to_string(), sr);
    }

    // 7. Bucket configs (depends on buckets)
    if let Some(configs_val) = body.get("bucket_configs") {
        let mut sr = SectionResult::default();
        if let Some(obj) = configs_val.as_object() {
            for (bucket_name, config) in obj {
                // Verify bucket exists.
                let bucket_exists = state
                    .metadata
                    .head_bucket(bucket_name)
                    .await
                    .unwrap_or(None)
                    .is_some();
                if !bucket_exists {
                    sr.errors += 1;
                    errors.push(format!("bucket_configs/{bucket_name}: bucket does not exist"));
                    continue;
                }

                if let Some(config_obj) = config.as_object() {
                    for (key, value) in config_obj {
                        if key == "tags" {
                            // Handle tags separately.
                            if let Some(tag_obj) = value.as_object() {
                                if !dry_run {
                                    let tags: Vec<(String, String)> = tag_obj
                                        .iter()
                                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                                        .collect();
                                    match state.metadata.put_bucket_tags(bucket_name, &tags).await {
                                        Ok(()) => sr.applied += 1,
                                        Err(e) => {
                                            sr.errors += 1;
                                            errors.push(format!("bucket_configs/{bucket_name}/tags: {e}"));
                                        }
                                    }
                                } else {
                                    sr.applied += 1;
                                }
                            }
                            continue;
                        }

                        // Only process known config keys.
                        if !BUCKET_CONFIG_KEYS.contains(&key.as_str()) {
                            continue;
                        }

                        let val_str = value.as_str().unwrap_or("").to_string();
                        let existing = state
                            .metadata
                            .get_bucket_config(bucket_name, key)
                            .await
                            .unwrap_or(None);

                        if existing.is_some() && !overwrite {
                            sr.skipped += 1;
                            continue;
                        }

                        if !dry_run {
                            match state.metadata.set_bucket_config(bucket_name, key, &val_str).await {
                                Ok(()) => sr.applied += 1,
                                Err(e) => {
                                    sr.errors += 1;
                                    errors.push(format!("bucket_configs/{bucket_name}/{key}: {e}"));
                                }
                            }
                        } else {
                            sr.applied += 1;
                        }
                    }
                }
            }
        }
        results.insert("bucket_configs".to_string(), sr);
    }

    Ok(Json(ImportResponse {
        mode: params.mode,
        results,
        errors,
    }))
}
