//! AMQP 0-9-1 (RabbitMQ) notification connector.
//!
//! Delivers S3 event notifications by publishing JSON payloads to an AMQP
//! exchange with a routing key. Uses the `lapin` crate (pure Rust).

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use lapin::options::{
    BasicPublishOptions, ExchangeDeclareOptions, QueueDeclareOptions,
};
use lapin::types::FieldTable;
use lapin::{BasicProperties, Connection, ConnectionProperties, ExchangeKind};

/// AMQP connector — delivers events by publishing to an AMQP exchange.
pub struct AmqpConnector {
    timeout: Duration,
}

impl AmqpConnector {
    /// Create a new AMQP connector with the given connection timeout.
    pub fn new(timeout: Duration) -> Self {
        AmqpConnector { timeout }
    }

    /// Extract the exchange name from properties. Default: empty string (default exchange).
    fn exchange_name(properties: &HashMap<String, String>) -> &str {
        properties
            .get("exchange")
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    /// Extract the routing key from properties. Default: "arca.notifications".
    fn routing_key(properties: &HashMap<String, String>) -> &str {
        properties
            .get("routing_key")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("arca.notifications")
    }

    /// Check if durable flag is set. Default: true.
    fn is_durable(properties: &HashMap<String, String>) -> bool {
        properties
            .get("durable")
            .map(|s| s.as_str())
            .map(|s| s != "false")
            .unwrap_or(true)
    }

    /// Connect to the AMQP broker with timeout.
    async fn connect(&self, destination: &str) -> Result<Connection, String> {
        // lapin 4 integrates with tokio natively: the executor/reactor
        // trait shims of lapin 2 are gone.
        let conn_props = ConnectionProperties::default();

        tokio::time::timeout(self.timeout, Connection::connect(destination, conn_props))
            .await
            .map_err(|_| "connection timeout".to_string())?
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl NotificationConnector for AmqpConnector {
    fn name(&self) -> &str {
        "amqp"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let exchange = Self::exchange_name(properties);
        let routing_key = Self::routing_key(properties);
        let durable = Self::is_durable(properties);

        let conn = match self.connect(destination).await {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        let channel = match conn.create_channel().await {
            Ok(ch) => ch,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "channel error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        };

        // Enable publisher confirms
        if let Err(e) = channel
            .confirm_select(lapin::options::ConfirmSelectOptions::default())
            .await
        {
            return DeliveryResult {
                success: false,
                status_info: "confirm_select error".to_string(),
                error: Some(e.to_string()),
            };
        }

        // Declare exchange if a non-default exchange is specified
        if !exchange.is_empty() {
            let opts = ExchangeDeclareOptions {
                durable,
                ..ExchangeDeclareOptions::default()
            };
            if let Err(e) = channel
                .exchange_declare(exchange.into(), ExchangeKind::Topic, opts, FieldTable::default())
                .await
            {
                return DeliveryResult {
                    success: false,
                    status_info: "exchange_declare error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        }

        // When using the default exchange, declare a queue named after the routing key
        // and bind it so messages are routable.
        if exchange.is_empty() {
            let opts = QueueDeclareOptions {
                durable,
                ..QueueDeclareOptions::default()
            };
            if let Err(e) = channel
                .queue_declare(routing_key.into(), opts, FieldTable::default())
                .await
            {
                return DeliveryResult {
                    success: false,
                    status_info: "queue_declare error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        }

        // Publish
        let props = BasicProperties::default()
            .with_content_type("application/json".into())
            .with_delivery_mode(if durable { 2 } else { 1 });

        let confirm = match channel
            .basic_publish(
                exchange.into(),
                routing_key.into(),
                BasicPublishOptions::default(),
                payload.as_bytes(),
                props,
            )
            .await
        {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "publish error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        };

        // Wait for publisher confirm
        match confirm.await {
            Ok(_) => DeliveryResult {
                success: true,
                status_info: format!(
                    "published to exchange='{}' routing_key='{}'",
                    if exchange.is_empty() { "(default)" } else { exchange },
                    routing_key,
                ),
                error: None,
            },
            Err(e) => DeliveryResult {
                success: false,
                status_info: "confirm error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        _properties: &HashMap<String, String>,
    ) -> TestResult {
        let conn = match self.connect(destination).await {
            Ok(c) => c,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        match conn.create_channel().await {
            Ok(_) => TestResult {
                success: true,
                status_info: "AMQP connection OK".to_string(),
                error: None,
            },
            Err(e) => TestResult {
                success: false,
                status_info: "channel error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exchange_name_default() {
        let props = HashMap::new();
        assert_eq!(AmqpConnector::exchange_name(&props), "");
    }

    #[test]
    fn test_exchange_name_custom() {
        let mut props = HashMap::new();
        props.insert("exchange".to_string(), "my-exchange".to_string());
        assert_eq!(AmqpConnector::exchange_name(&props), "my-exchange");
    }

    #[test]
    fn test_routing_key_default() {
        let props = HashMap::new();
        assert_eq!(AmqpConnector::routing_key(&props), "arca.notifications");
    }

    #[test]
    fn test_routing_key_custom() {
        let mut props = HashMap::new();
        props.insert("routing_key".to_string(), "custom.key".to_string());
        assert_eq!(AmqpConnector::routing_key(&props), "custom.key");
    }

    #[test]
    fn test_is_durable_default() {
        let props = HashMap::new();
        assert!(AmqpConnector::is_durable(&props));
    }

    #[test]
    fn test_is_durable_false() {
        let mut props = HashMap::new();
        props.insert("durable".to_string(), "false".to_string());
        assert!(!AmqpConnector::is_durable(&props));
    }

    #[test]
    fn test_connector_name() {
        let connector = AmqpConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "amqp");
    }

    #[tokio::test]
    async fn test_deliver_connection_refused() {
        let connector = AmqpConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("amqp://127.0.0.1:1", "{}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_test_connection_refused() {
        let connector = AmqpConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector.test("amqp://127.0.0.1:1", &props).await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
