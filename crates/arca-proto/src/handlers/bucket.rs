//! Bucket operation handlers.
//!
//! All handlers return NotImplemented (501) for Phase 0.

use axum::extract::Path;
use axum::response::Response;

use crate::xml::error_response::not_implemented_response;

pub async fn list_buckets() -> Response {
    not_implemented_response("/")
}

pub async fn get_bucket(Path(bucket): Path<String>) -> Response {
    not_implemented_response(&format!("/{bucket}"))
}

pub async fn head_bucket(Path(bucket): Path<String>) -> Response {
    not_implemented_response(&format!("/{bucket}"))
}

pub async fn create_bucket(Path(bucket): Path<String>) -> Response {
    not_implemented_response(&format!("/{bucket}"))
}

pub async fn delete_bucket(Path(bucket): Path<String>) -> Response {
    not_implemented_response(&format!("/{bucket}"))
}
