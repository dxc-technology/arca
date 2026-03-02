//! Object operation handlers.
//!
//! All handlers return NotImplemented (501) for now.

use axum::extract::{Path, State};
use axum::response::Response;

use crate::state::AppState;
use crate::xml::error_response::not_implemented_response;

pub async fn get_object(
    State(_state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn head_object(
    State(_state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn put_object(
    State(_state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn delete_object(
    State(_state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn post_object(
    State(_state): State<AppState>,
    Path((bucket, key)): Path<(String, String)>,
) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}
