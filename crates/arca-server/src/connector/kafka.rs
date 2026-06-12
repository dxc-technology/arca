//! Kafka notification connector.
//!
//! Delivers S3 event notifications by producing JSON payloads to a Kafka topic.
//! Uses the `rdkafka` crate (librdkafka wrapper) with cmake-build for static linking.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use rdkafka::config::ClientConfig;
use rdkafka::producer::{FutureProducer, FutureRecord};

/// Kafka connector — delivers events by producing messages to a Kafka topic.
pub struct KafkaConnector {
    timeout: Duration,
}

impl KafkaConnector {
    /// Create a new Kafka connector with the given timeout.
    pub fn new(timeout: Duration) -> Self {
        KafkaConnector { timeout }
    }

    /// Extract the topic name from properties. Default: "arca-notifications".
    fn topic_name(properties: &HashMap<String, String>) -> &str {
        properties
            .get("topic")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("arca-notifications")
    }

    /// Extract the security protocol from properties. Default: "plaintext".
    fn security_protocol(properties: &HashMap<String, String>) -> &str {
        properties
            .get("security_protocol")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("plaintext")
    }

    /// Build a Kafka `ClientConfig` from the destination and properties.
    fn build_config(
        destination: &str,
        properties: &HashMap<String, String>,
        timeout: Duration,
    ) -> ClientConfig {
        let mut config = ClientConfig::new();
        config.set("bootstrap.servers", destination);
        config.set("message.timeout.ms", timeout.as_millis().to_string());

        let protocol = Self::security_protocol(properties);
        config.set("security.protocol", protocol);

        // Optional SASL authentication
        if let Some(username) = properties.get("sasl_username").filter(|s| !s.is_empty()) {
            config.set("sasl.username", username);
            config.set("sasl.mechanism", "PLAIN");
        }
        if let Some(password) = properties.get("sasl_password").filter(|s| !s.is_empty()) {
            config.set("sasl.password", password);
        }

        config
    }
}

#[async_trait::async_trait]
impl NotificationConnector for KafkaConnector {
    fn name(&self) -> &str {
        "kafka"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let topic = Self::topic_name(properties);
        let config = Self::build_config(destination, properties, self.timeout);

        let producer: FutureProducer = match config.create() {
            Ok(p) => p,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "producer creation error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        };

        let record = FutureRecord::to(topic)
            .payload(payload)
            .key("arca");

        match producer.send(record, self.timeout).await {
            Ok(delivery) => DeliveryResult {
                success: true,
                status_info: format!(
                    "produced to '{topic}' (partition={}, offset={})",
                    delivery.partition, delivery.offset
                ),
                error: None,
            },
            Err((e, _)) => DeliveryResult {
                success: false,
                status_info: "produce error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let topic = Self::topic_name(properties);
        let config = Self::build_config(destination, properties, self.timeout);

        let producer: FutureProducer = match config.create() {
            Ok(p) => p,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "producer creation error".to_string(),
                    error: Some(e.to_string()),
                };
            }
        };

        // Fetch metadata to verify broker connectivity
        match tokio::time::timeout(self.timeout, tokio::task::spawn_blocking({
            let timeout = self.timeout;
            let topic = topic.to_string();
            move || {
                use rdkafka::producer::Producer;
                producer
                    .client()
                    .fetch_metadata(Some(&topic), timeout)
            }
        }))
        .await
        {
            Ok(Ok(Ok(metadata))) => {
                let broker_count = metadata.brokers().len();
                TestResult {
                    success: true,
                    status_info: format!("Kafka cluster OK ({broker_count} brokers)"),
                    error: None,
                }
            }
            Ok(Ok(Err(e))) => TestResult {
                success: false,
                status_info: "metadata fetch error".to_string(),
                error: Some(e.to_string()),
            },
            Ok(Err(e)) => TestResult {
                success: false,
                status_info: "task error".to_string(),
                error: Some(e.to_string()),
            },
            Err(_) => TestResult {
                success: false,
                status_info: "connection timeout".to_string(),
                error: Some("metadata fetch timed out".to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_name_default() {
        let props = HashMap::new();
        assert_eq!(KafkaConnector::topic_name(&props), "arca-notifications");
    }

    #[test]
    fn test_topic_name_custom() {
        let mut props = HashMap::new();
        props.insert("topic".to_string(), "my-topic".to_string());
        assert_eq!(KafkaConnector::topic_name(&props), "my-topic");
    }

    #[test]
    fn test_security_protocol_default() {
        let props = HashMap::new();
        assert_eq!(KafkaConnector::security_protocol(&props), "plaintext");
    }

    #[test]
    fn test_security_protocol_custom() {
        let mut props = HashMap::new();
        props.insert("security_protocol".to_string(), "sasl_plaintext".to_string());
        assert_eq!(KafkaConnector::security_protocol(&props), "sasl_plaintext");
    }

    #[test]
    fn test_connector_name() {
        let connector = KafkaConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "kafka");
    }

    #[test]
    fn test_build_config_basic() {
        let props = HashMap::new();
        let config = KafkaConnector::build_config("localhost:9092", &props, Duration::from_secs(5));
        // Just verify it can be created without panicking
        let _: FutureProducer = config.create().expect("should create producer from basic config");
    }

    #[test]
    fn test_build_config_with_sasl() {
        let mut props = HashMap::new();
        props.insert("sasl_username".to_string(), "user".to_string());
        props.insert("sasl_password".to_string(), "pass".to_string());
        props.insert("security_protocol".to_string(), "sasl_plaintext".to_string());
        let config = KafkaConnector::build_config("localhost:9092", &props, Duration::from_secs(5));
        let _: FutureProducer = config.create().expect("should create producer with SASL config");
    }
}
