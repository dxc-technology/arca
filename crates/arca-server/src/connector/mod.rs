//! Notification connector implementations.
//!
//! Each connector implements the `NotificationConnector` trait from `arca-core`.
//! Phase 25 introduced the `WebhookConnector`; Phase 26 adds additional connectors
//! (Redis, NATS, Kafka, AMQP, MQTT, PostgreSQL, MySQL, MongoDB, Elasticsearch,
//! Syslog, SMTP, gRPC, ONVIF).

pub mod db_common;
pub mod mongodb;
pub mod mqtt;
pub mod mysql;
pub mod nats;
pub mod postgresql;
pub mod redis;
pub mod webhook;

pub use self::mongodb::MongodbConnector;
pub use mqtt::MqttConnector;
pub use mysql::MysqlConnector;
pub use nats::NatsConnector;
pub use postgresql::PostgresqlConnector;
pub use redis::RedisConnector;
pub use webhook::WebhookConnector;
