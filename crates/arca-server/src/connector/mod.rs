//! Notification connector implementations.
//!
//! Each connector implements the `NotificationConnector` trait from `arca-core`.
//! Phase 25 introduced the `WebhookConnector`; Phase 26 adds additional connectors
//! (Redis, NATS, Kafka, AMQP, MQTT, PostgreSQL, MySQL, MongoDB, Elasticsearch,
//! Syslog, SMTP, gRPC).

pub mod amqp;
pub mod db_common;
pub mod elasticsearch;
pub mod grpc;
pub mod kafka;
pub mod mongodb;
pub mod mqtt;
pub mod mysql;
pub mod nats;
pub mod postgresql;
pub mod redis;
pub mod smtp;
pub mod syslog;
pub mod webhook;

pub use amqp::AmqpConnector;
pub use elasticsearch::ElasticsearchConnector;
pub use grpc::GrpcConnector;
pub use kafka::KafkaConnector;
pub use self::mongodb::MongodbConnector;
pub use mqtt::MqttConnector;
pub use mysql::MysqlConnector;
pub use nats::NatsConnector;
pub use postgresql::PostgresqlConnector;
pub use redis::RedisConnector;
pub use smtp::SmtpConnector;
pub use syslog::SyslogConnector;
pub use webhook::WebhookConnector;
