//! Notification connector implementations.
//!
//! Each connector implements the `NotificationConnector` trait from `arca-core`.
//! Phase 25 introduced the `WebhookConnector`; Phase 26 adds additional connectors
//! (Redis, NATS, Kafka, AMQP, MQTT, PostgreSQL, MySQL, MongoDB, Elasticsearch,
//! Syslog, SMTP, gRPC, ONVIF).

pub mod redis;
pub mod webhook;

pub use redis::RedisConnector;
pub use webhook::WebhookConnector;
