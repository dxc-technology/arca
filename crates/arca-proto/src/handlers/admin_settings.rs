//! Admin API handlers for instance-wide settings.
//!
//! Settings follow a TOML > DB > default precedence chain. When a setting
//! is defined in the TOML config file, it is read-only from the console.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::handlers::admin::AdminError;
use crate::state::AppState;

/// Default region when neither TOML nor DB specifies one.
const DEFAULT_REGION: &str = "us-east-1";

/// Default audit log retention in days.
const DEFAULT_AUDIT_RETENTION_DAYS: u32 = 90;

/// Default metrics retention in days.
const DEFAULT_METRICS_RETENTION_DAYS: u32 = 30;

/// Default notification event retention in days.
const DEFAULT_NOTIFICATION_RETENTION_DAYS: u32 = 7;

/// Default lifecycle evaluation interval in seconds (1 hour).
const DEFAULT_LIFECYCLE_EVALUATION_INTERVAL: u64 = 3600;

/// Default preview max size in MB for images/PDF/HTML (0 = unlimited).
const DEFAULT_PREVIEW_MAX_SIZE_MB: u32 = 10;

/// Default preview max size in MB for text/markdown (0 = unlimited).
const DEFAULT_PREVIEW_MAX_TEXT_MB: u32 = 1;

/// Default preview max size in MB for video (0 = unlimited).
const DEFAULT_PREVIEW_MAX_VIDEO_MB: u32 = 100;

/// Known setting keys.
const KNOWN_SETTINGS: &[&str] = &[
    "region",
    "audit_retention_days",
    "notification_retention_days",
    "metrics_retention_days",
    "lifecycle_evaluation_interval",
    "preview_max_size_mb",
    "preview_max_text_mb",
    "preview_max_video_mb",
];

/// A single setting with its effective value and source.
#[derive(Serialize)]
struct SettingValue {
    value: String,
    source: &'static str,
    readonly: bool,
}

/// Response for GET /admin/settings.
#[derive(Serialize)]
struct SettingsResponse {
    region: SettingValue,
    audit_retention_days: SettingValue,
    notification_retention_days: SettingValue,
    metrics_retention_days: SettingValue,
    lifecycle_evaluation_interval: SettingValue,
    preview_max_size_mb: SettingValue,
    preview_max_text_mb: SettingValue,
    preview_max_video_mb: SettingValue,
}

/// Request body for PUT /admin/settings/{key}.
#[derive(Deserialize)]
pub struct SetSettingRequest {
    value: String,
}

/// Resolve a setting's effective value and source.
async fn resolve_setting(
    state: &AppState,
    key: &str,
) -> Result<SettingValue, AdminError> {
    match key {
        "region" => {
            if let Some(ref region) = state.config_region {
                Ok(SettingValue {
                    value: region.clone(),
                    source: "config_file",
                    readonly: true,
                })
            } else if let Ok(Some(val)) = state.server_config.get_server_config("region").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_REGION.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        "audit_retention_days" => {
            if let Some(days) = state.config_audit_retention_days {
                Ok(SettingValue {
                    value: days.to_string(),
                    source: "config_file",
                    readonly: true,
                })
            } else if let Ok(Some(val)) = state.server_config.get_server_config("audit_retention_days").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_AUDIT_RETENTION_DAYS.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        "metrics_retention_days" => {
            if let Some(days) = state.config_metrics_retention_days {
                Ok(SettingValue {
                    value: days.to_string(),
                    source: "config_file",
                    readonly: true,
                })
            } else if let Ok(Some(val)) = state.server_config.get_server_config("metrics_retention_days").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_METRICS_RETENTION_DAYS.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        "notification_retention_days" => {
            if let Some(days) = state.config_notification_retention_days {
                Ok(SettingValue {
                    value: days.to_string(),
                    source: "config_file",
                    readonly: true,
                })
            } else if let Ok(Some(val)) = state.server_config.get_server_config("notification_retention_days").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_NOTIFICATION_RETENTION_DAYS.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        "lifecycle_evaluation_interval" => {
            if let Ok(Some(val)) = state.server_config.get_server_config("lifecycle_evaluation_interval").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_LIFECYCLE_EVALUATION_INTERVAL.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        "preview_max_size_mb" => {
            if let Ok(Some(val)) = state.server_config.get_server_config("preview_max_size_mb").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_PREVIEW_MAX_SIZE_MB.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        "preview_max_text_mb" => {
            if let Ok(Some(val)) = state.server_config.get_server_config("preview_max_text_mb").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_PREVIEW_MAX_TEXT_MB.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        "preview_max_video_mb" => {
            if let Ok(Some(val)) = state.server_config.get_server_config("preview_max_video_mb").await {
                Ok(SettingValue {
                    value: val,
                    source: "database",
                    readonly: false,
                })
            } else {
                Ok(SettingValue {
                    value: DEFAULT_PREVIEW_MAX_VIDEO_MB.to_string(),
                    source: "default",
                    readonly: false,
                })
            }
        }
        _ => Err(AdminError::not_found(format!("Unknown setting: {key}"))),
    }
}

/// GET /admin/settings — returns all effective settings.
pub async fn list_settings(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, AdminError> {
    let region = resolve_setting(&state, "region").await?;
    let audit_retention_days = resolve_setting(&state, "audit_retention_days").await?;
    let notification_retention_days = resolve_setting(&state, "notification_retention_days").await?;
    let metrics_retention_days = resolve_setting(&state, "metrics_retention_days").await?;
    let lifecycle_evaluation_interval = resolve_setting(&state, "lifecycle_evaluation_interval").await?;
    let preview_max_size_mb = resolve_setting(&state, "preview_max_size_mb").await?;
    let preview_max_text_mb = resolve_setting(&state, "preview_max_text_mb").await?;
    let preview_max_video_mb = resolve_setting(&state, "preview_max_video_mb").await?;

    Ok(Json(SettingsResponse {
        region,
        audit_retention_days,
        notification_retention_days,
        metrics_retention_days,
        lifecycle_evaluation_interval,
        preview_max_size_mb,
        preview_max_text_mb,
        preview_max_video_mb,
    }))
}

/// PUT /admin/settings/{key} — set a server config value.
pub async fn update_setting(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Json(body): Json<SetSettingRequest>,
) -> Result<impl IntoResponse, AdminError> {
    // Validate key is known
    if !KNOWN_SETTINGS.contains(&key.as_str()) {
        return Err(AdminError::not_found(format!("Unknown setting: {key}")));
    }

    // Check if locked by config file
    let current = resolve_setting(&state, &key).await?;
    if current.readonly {
        return Err(AdminError::conflict(format!(
            "Setting \"{key}\" is locked by the configuration file and cannot be changed from the console"
        )));
    }

    // Validate the value
    validate_setting_value(&key, &body.value)?;

    // Store in DB
    state
        .server_config
        .set_server_config(&key, &body.value)
        .await
        .map_err(|e| AdminError::internal(format!("Failed to save setting: {e}")))?;

    // Return the updated setting
    let updated = resolve_setting(&state, &key).await?;
    Ok(Json(serde_json::json!({
        "key": key,
        "value": updated.value,
        "source": updated.source,
        "readonly": updated.readonly,
    })))
}

/// DELETE /admin/settings/{key} — reset a setting to its default.
pub async fn delete_setting(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    if !KNOWN_SETTINGS.contains(&key.as_str()) {
        return Err(AdminError::not_found(format!("Unknown setting: {key}")));
    }

    let current = resolve_setting(&state, &key).await?;
    if current.readonly {
        return Err(AdminError::conflict(format!(
            "Setting \"{key}\" is locked by the configuration file and cannot be reset from the console"
        )));
    }

    state
        .server_config
        .delete_server_config(&key)
        .await
        .map_err(|e| AdminError::internal(format!("Failed to delete setting: {e}")))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Validate a setting value before storing.
fn validate_setting_value(key: &str, value: &str) -> Result<(), AdminError> {
    match key {
        "region" => {
            if value.is_empty() {
                return Err(AdminError::bad_request("Region cannot be empty"));
            }
            // Basic format: lowercase letters, digits, hyphens
            if !value.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
                return Err(AdminError::bad_request(
                    "Region must contain only lowercase letters, digits, and hyphens",
                ));
            }
            Ok(())
        }
        "audit_retention_days" | "metrics_retention_days" => {
            let days: u32 = value.parse().map_err(|_| {
                AdminError::bad_request(format!("{key} must be a non-negative integer"))
            })?;
            if days > 3650 {
                return Err(AdminError::bad_request(format!(
                    "{key} cannot exceed 3650 (10 years)"
                )));
            }
            Ok(())
        }
        "lifecycle_evaluation_interval" => {
            let secs: u64 = value.parse().map_err(|_| {
                AdminError::bad_request("lifecycle_evaluation_interval must be a positive integer (seconds)")
            })?;
            if secs < 60 {
                return Err(AdminError::bad_request(
                    "lifecycle_evaluation_interval cannot be less than 60 seconds",
                ));
            }
            if secs > 86400 {
                return Err(AdminError::bad_request(
                    "lifecycle_evaluation_interval cannot exceed 86400 seconds (24 hours)",
                ));
            }
            Ok(())
        }
        "preview_max_size_mb" | "preview_max_text_mb" | "preview_max_video_mb" => {
            let mb: u32 = value.parse().map_err(|_| {
                AdminError::bad_request(format!("{key} must be a non-negative integer"))
            })?;
            if mb > 10240 {
                return Err(AdminError::bad_request(format!(
                    "{key} cannot exceed 10240 MB (10 GB)"
                )));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Helper: resolve the effective region for S3 operations.
/// Per-bucket region (bucket_config) > instance-wide (TOML or DB) > default.
pub async fn effective_region(state: &AppState, bucket: Option<&str>) -> String {
    // Check per-bucket region
    if let Some(bucket) = bucket {
        if let Ok(Some(region)) = state.metadata.get_bucket_config(bucket, "region").await {
            return region;
        }
    }

    // Instance-wide: TOML takes precedence
    if let Some(ref region) = state.config_region {
        return region.clone();
    }

    // Instance-wide: DB
    if let Ok(Some(region)) = state.server_config.get_server_config("region").await {
        return region;
    }

    // Hard default
    DEFAULT_REGION.to_string()
}

/// Helper: resolve the effective audit retention days.
pub async fn effective_audit_retention_days(state: &AppState) -> u32 {
    if let Some(days) = state.config_audit_retention_days {
        return days;
    }
    if let Ok(Some(val)) = state.server_config.get_server_config("audit_retention_days").await {
        if let Ok(days) = val.parse::<u32>() {
            return days;
        }
    }
    DEFAULT_AUDIT_RETENTION_DAYS
}

/// Helper: resolve the effective metrics retention days.
pub async fn effective_metrics_retention_days(state: &AppState) -> u32 {
    if let Some(days) = state.config_metrics_retention_days {
        return days;
    }
    if let Ok(Some(val)) = state.server_config.get_server_config("metrics_retention_days").await {
        if let Ok(days) = val.parse::<u32>() {
            return days;
        }
    }
    DEFAULT_METRICS_RETENTION_DAYS
}
