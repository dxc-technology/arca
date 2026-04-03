//! Admin API handlers for notification events.

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use arca_core::store::notification::NotificationEventFilter;

use crate::handlers::admin::AdminError;
use crate::state::AppState;

/// Query parameters for GET /admin/notifications/events.
#[derive(Debug, Deserialize)]
pub struct NotificationEventQueryParams {
    pub bucket: Option<String>,
    pub event_name: Option<String>,
    pub delivery_status: Option<String>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
}

/// GET /admin/notifications/events — list notification events with optional filters.
pub async fn list_notification_events(
    State(state): State<AppState>,
    Query(params): Query<NotificationEventQueryParams>,
) -> Result<impl IntoResponse, AdminError> {
    let store = state
        .notification_store
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Notification store is not available"))?;

    let filter = NotificationEventFilter {
        bucket: params.bucket,
        event_name: params.event_name,
        delivery_status: params.delivery_status,
        offset: params.offset.unwrap_or(0),
        limit: params.limit.unwrap_or(100).min(1000),
    };

    let entries = store
        .list_notification_events(&filter)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    let total = store
        .count_notification_events(&filter)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "entries": entries,
        "total": total,
        "offset": filter.offset,
        "limit": filter.limit,
    })))
}

/// GET /admin/notifications/events/count — count notification events.
pub async fn count_notification_events(
    State(state): State<AppState>,
    Query(params): Query<NotificationEventQueryParams>,
) -> Result<impl IntoResponse, AdminError> {
    let store = state
        .notification_store
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Notification store is not available"))?;

    let filter = NotificationEventFilter {
        bucket: params.bucket,
        event_name: params.event_name,
        delivery_status: params.delivery_status,
        offset: 0,
        limit: 0,
    };

    let total = store
        .count_notification_events(&filter)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({ "count": total })))
}

/// Request body for DELETE /admin/notifications/events.
#[derive(Debug, Deserialize)]
pub struct ClearEventsRequest {
    pub confirm: String,
}

/// DELETE /admin/notifications/events — delete all notification events.
pub async fn clear_notification_events(
    State(state): State<AppState>,
    Json(body): Json<ClearEventsRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if body.confirm != "CLEAR EVENTS" {
        return Err(AdminError::bad_request(
            "Confirmation required: send {\"confirm\": \"CLEAR EVENTS\"}",
        ));
    }

    let store = state
        .notification_store
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Notification store is not available"))?;

    // Purge everything (use a far-future cutoff)
    let cutoff = chrono::Utc::now() + chrono::Duration::days(1);
    let deleted = store
        .purge_notification_events(cutoff)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(serde_json::json!({
        "deleted": deleted,
    })))
}

/// Request body for POST /admin/notifications/test-webhook (backward-compatible alias).
#[derive(Debug, Deserialize)]
pub struct TestWebhookRequest {
    pub url: String,
    /// Optional auth token for webhook Bearer authentication.
    pub auth_token: Option<String>,
}

/// POST /admin/notifications/test-webhook — send a test event to verify webhook connectivity.
/// Backward-compatible alias that delegates to the connector registry.
pub async fn test_webhook(
    State(state): State<AppState>,
    Json(body): Json<TestWebhookRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if body.url.is_empty() {
        return Err(AdminError::bad_request("URL must not be empty"));
    }

    let registry = state
        .connector_registry
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Connector registry is not available"))?;

    let connector = registry
        .get(&arca_core::s3::notification::ConnectorType::Webhook)
        .ok_or_else(|| AdminError::internal("Webhook connector not registered".to_string()))?;

    let mut properties = std::collections::HashMap::new();
    if let Some(token) = body.auth_token {
        if !token.is_empty() {
            properties.insert("auth_token".to_string(), token);
        }
    }

    let result = connector.test(&body.url, &properties).await;

    Ok(Json(serde_json::json!({
        "success": result.success,
        "status": result.status_info,
        "error": result.error,
    })))
}

/// Request body for POST /admin/notifications/test-connector.
#[derive(Debug, Deserialize)]
pub struct TestConnectorRequest {
    /// Connector type (e.g. "webhook", "kafka").
    pub connector_type: String,
    /// Destination address (URL, broker, etc.).
    pub url: String,
    /// Connector-specific properties (auth_token, topic, etc.).
    #[serde(default)]
    pub properties: std::collections::HashMap<String, String>,
}

/// POST /admin/notifications/test-connector — test any registered connector.
pub async fn test_connector(
    State(state): State<AppState>,
    Json(body): Json<TestConnectorRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if body.url.is_empty() {
        return Err(AdminError::bad_request("URL must not be empty"));
    }

    let ct: arca_core::s3::notification::ConnectorType =
        serde_json::from_value(serde_json::Value::String(body.connector_type.clone()))
            .map_err(|_| {
                AdminError::bad_request(&format!(
                    "Unknown connector type: {}",
                    body.connector_type
                ))
            })?;

    let registry = state
        .connector_registry
        .as_ref()
        .ok_or_else(|| AdminError::bad_request("Connector registry is not available"))?;

    let connector = registry.get(&ct).ok_or_else(|| {
        AdminError::bad_request(&format!(
            "Connector '{}' is not available (not implemented yet)",
            body.connector_type
        ))
    })?;

    let result = connector.test(&body.url, &body.properties).await;

    Ok(Json(serde_json::json!({
        "success": result.success,
        "connector_type": body.connector_type,
        "status": result.status_info,
        "error": result.error,
    })))
}
