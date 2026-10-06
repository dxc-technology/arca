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

/// Builds a connector's HTTP client without panicking. Building fails when the
/// system trust store cannot be loaded (a host without a CA bundle, e.g. a bare
/// binary in a `FROM scratch` image): the connector then keeps the error and
/// reports it on every use, so the server still starts. Logs one warning.
pub(crate) fn build_http_client(
    connector: &str,
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, String> {
    builder.build().map_err(|e| {
        let mut cause = e.to_string();
        let mut source = std::error::Error::source(&e);
        while let Some(inner) = source {
            cause = format!("{cause}: {inner}");
            source = inner.source();
        }
        tracing::warn!(
            connector,
            error = %cause,
            "HTTP client unavailable: the connector will fail every delivery"
        );
        cause
    })
}

/// The connector's client, or the reason it could not be built.
pub(crate) fn usable_client(client: &Result<reqwest::Client, String>) -> Result<&reqwest::Client, String> {
    client
        .as_ref()
        .map_err(|e| format!("HTTP client unavailable: {e}"))
}
