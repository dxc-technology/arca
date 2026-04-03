//! Notification connector implementations.
//!
//! Each connector implements the `NotificationConnector` trait from `arca-core`.
//! Currently only the `WebhookConnector` is implemented; future connectors
//! (Kafka, Redis, MongoDB, etc.) will be added in Phase 26.

pub mod webhook;

pub use webhook::WebhookConnector;
