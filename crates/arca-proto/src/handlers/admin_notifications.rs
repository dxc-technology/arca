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

/// Request body for POST /admin/notifications/test-webhook.
#[derive(Debug, Deserialize)]
pub struct TestWebhookRequest {
    pub url: String,
}

/// POST /admin/notifications/test-webhook — send a test event to verify connectivity.
pub async fn test_webhook(
    Json(body): Json<TestWebhookRequest>,
) -> Result<impl IntoResponse, AdminError> {
    if body.url.is_empty() {
        return Err(AdminError::bad_request("URL must not be empty"));
    }

    let test_event = serde_json::json!({
        "Records": [{
            "eventVersion": "2.1",
            "eventSource": "arca:s3",
            "awsRegion": "test",
            "eventTime": chrono::Utc::now().to_rfc3339(),
            "eventName": "s3:TestEvent",
            "userIdentity": { "principalId": "test" },
            "requestParameters": { "sourceIPAddress": "127.0.0.1" },
            "responseElements": { "x-amz-request-id": "test", "x-amz-id-2": "" },
            "s3": {
                "s3SchemaVersion": "1.0",
                "configurationId": "test",
                "bucket": { "name": "test-bucket", "ownerIdentity": { "principalId": "test" }, "arn": "arn:arca:s3:::test-bucket" },
                "object": { "key": "test-key", "size": 0, "eTag": "", "sequencer": "000" }
            }
        }]
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AdminError::internal(format!("Failed to create HTTP client: {e}")))?;

    match client
        .post(&body.url)
        .header("Content-Type", "application/json")
        .json(&test_event)
        .send()
        .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let success = resp.status().is_success();
            Ok(Json(serde_json::json!({
                "success": success,
                "status": status,
            })))
        }
        Err(e) => Ok(Json(serde_json::json!({
            "success": false,
            "error": e.to_string(),
        }))),
    }
}
