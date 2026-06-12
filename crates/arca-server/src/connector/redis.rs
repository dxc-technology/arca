//! Redis Pub/Sub notification connector.
//!
//! Delivers S3 event notifications by publishing JSON payloads to a Redis
//! Pub/Sub channel. Supports optional authentication via the `password`
//! property or via credentials embedded in the Redis URL.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};

/// Redis Pub/Sub connector — delivers events via PUBLISH to a Redis channel.
pub struct RedisConnector {
    timeout: Duration,
}

impl RedisConnector {
    /// Create a new Redis connector with the given connection timeout.
    pub fn new(timeout: Duration) -> Self {
        RedisConnector { timeout }
    }

    /// Extract the channel name from properties, defaulting to "arca:notifications".
    fn channel_name(properties: &HashMap<String, String>) -> &str {
        properties
            .get("channel")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("arca:notifications")
    }

    /// Open an async multiplexed connection to the given Redis URL.
    async fn connect(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> Result<redis::aio::MultiplexedConnection, redis::RedisError> {
        // Build connection info from destination URL, optionally overriding password.
        let info: redis::ConnectionInfo = destination.parse()?;

        // redis 1.x made the connection-info fields private: overrides go
        // through the consuming builder setters.
        let mut redis_settings = info.redis_settings().clone();
        if let Some(password) = properties.get("password").filter(|p| !p.is_empty()) {
            redis_settings = redis_settings.set_password(password);
        }
        if let Some(db) = properties.get("db").and_then(|d| d.parse::<i64>().ok()) {
            redis_settings = redis_settings.set_db(db);
        }
        let info = info.set_redis_settings(redis_settings);

        let client = redis::Client::open(info)?;
        tokio::time::timeout(
            self.timeout,
            client.get_multiplexed_async_connection(),
        )
        .await
        .map_err(|_| {
            redis::RedisError::from((
                redis::ErrorKind::Io,
                "connection timeout",
            ))
        })?
    }
}

#[async_trait::async_trait]
impl NotificationConnector for RedisConnector {
    fn name(&self) -> &str {
        "redis"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let channel = Self::channel_name(properties);

        let mut conn = match self.connect(destination, properties).await {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        };

        match redis::cmd("PUBLISH")
            .arg(channel)
            .arg(payload)
            .query_async::<i64>(&mut conn)
            .await
        {
            Ok(subscribers) => DeliveryResult {
                success: true,
                status_info: format!("PUBLISH to '{channel}' ({subscribers} subscribers)"),
                error: None,
            },
            Err(e) => DeliveryResult {
                success: false,
                status_info: "publish error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let mut conn = match self.connect(destination, properties).await {
            Ok(c) => c,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        };

        match redis::cmd("PING")
            .query_async::<String>(&mut conn)
            .await
        {
            Ok(resp) if resp == "PONG" => TestResult {
                success: true,
                status_info: "PONG".to_string(),
                error: None,
            },
            Ok(resp) => TestResult {
                success: false,
                status_info: format!("unexpected response: {resp}"),
                error: Some(format!("expected PONG, got: {resp}")),
            },
            Err(e) => TestResult {
                success: false,
                status_info: "ping error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_name_from_properties() {
        let mut props = HashMap::new();
        assert_eq!(RedisConnector::channel_name(&props), "arca:notifications");

        props.insert("channel".to_string(), "my-channel".to_string());
        assert_eq!(RedisConnector::channel_name(&props), "my-channel");

        props.insert("channel".to_string(), "".to_string());
        assert_eq!(RedisConnector::channel_name(&props), "arca:notifications");
    }

    #[test]
    fn test_connector_name() {
        let connector = RedisConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "redis");
    }

    #[tokio::test]
    async fn test_deliver_connection_refused() {
        let connector = RedisConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("redis://127.0.0.1:1", "{}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_test_connection_refused() {
        let connector = RedisConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector.test("redis://127.0.0.1:1", &props).await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_invalid_url() {
        let connector = RedisConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("not-a-valid-url", "{}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
