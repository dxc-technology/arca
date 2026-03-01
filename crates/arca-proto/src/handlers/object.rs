//! Object operation handlers.
//!
//! All handlers return NotImplemented (501) for Phase 0.

use axum::extract::Path;
use axum::response::Response;

use crate::xml::error_response::not_implemented_response;

pub async fn get_object(Path((bucket, key)): Path<(String, String)>) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn head_object(Path((bucket, key)): Path<(String, String)>) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn put_object(Path((bucket, key)): Path<(String, String)>) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn delete_object(Path((bucket, key)): Path<(String, String)>) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}

pub async fn post_object(Path((bucket, key)): Path<(String, String)>) -> Response {
    not_implemented_response(&format!("/{bucket}/{key}"))
}
