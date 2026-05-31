//! HTTP middleware for the S3 protocol layer.

pub mod admin_auth;
pub mod audit;
pub mod auth;
pub mod cluster_auth;
pub mod identity;
pub mod normalize;
pub mod rate_limit;
pub mod request_id;
pub mod validate;
pub mod virtual_host;
