//! Admin API handlers for presigned URL tracking.
//!
//! Provides visibility into active presigned URLs for the console.
//! The full URL is NOT stored or returned (security: avoid persisting bearer tokens).

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use super::admin::AdminError;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ListPresignedUrlsQuery {
    bucket: String,
}

/// GET /admin/presigned-urls?bucket=X — list active presigned URLs for a bucket.
pub async fn list_presigned_urls(
    State(state): State<AppState>,
    Query(query): Query<ListPresignedUrlsQuery>,
) -> Result<impl IntoResponse, AdminError> {
    let store = state
        .presigned_url_store
        .as_ref()
        .ok_or_else(|| AdminError::internal("presigned URL tracking not available"))?;

    let records = store
        .list_presigned_urls(&query.bucket)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    Ok(Json(records))
}

/// DELETE /admin/presigned-urls/:id — remove a presigned URL tracking record.
pub async fn delete_presigned_url(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AdminError> {
    let store = state
        .presigned_url_store
        .as_ref()
        .ok_or_else(|| AdminError::internal("presigned URL tracking not available"))?;

    let deleted = store
        .delete_presigned_url(&id)
        .await
        .map_err(|e| AdminError::internal(e.to_string()))?;

    if deleted {
        Ok(http::StatusCode::NO_CONTENT.into_response())
    } else {
        Err(AdminError::not_found("presigned URL record not found"))
    }
}
