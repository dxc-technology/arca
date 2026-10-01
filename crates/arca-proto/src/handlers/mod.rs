//! S3 request handlers.

pub mod admin;
pub mod admin_export;
pub mod admin_grants;
pub mod admin_import;
pub mod admin_monitoring;
pub mod admin_notifications;
pub mod admin_presigned_urls;
pub mod admin_proxy;
pub mod admin_replication;
pub mod admin_settings;
pub mod admin_teams;
pub mod admin_users;
pub mod archive;
pub mod body;
pub mod bucket;
pub mod cluster;
pub mod integrity;
pub mod maintenance;
pub mod multipart;
pub mod object;
mod sidecar;
pub mod ssec;
