//! MQTT notification connector.
//!
//! Delivers S3 event notifications by publishing JSON payloads to an MQTT
//! topic. Supports optional authentication via `username`/`password`
//! properties and configurable QoS level.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};

/// MQTT connector — delivers events via PUBLISH to an MQTT topic.
pub struct MqttConnector {
    timeout: Duration,
}

impl MqttConnector {
    /// Create a new MQTT connector with the given connection timeout.
    pub fn new(timeout: Duration) -> Self {
        MqttConnector { timeout }
    }

    /// Extract the topic name from properties, defaulting to "arca/notifications".
    fn topic_name(properties: &HashMap<String, String>) -> &str {
        properties
            .get("topic")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("arca/notifications")
    }

    /// Extract the QoS level from properties, defaulting to AtLeastOnce (1).
    fn qos_level(properties: &HashMap<String, String>) -> QoS {
        properties
            .get("qos")
            .and_then(|q| q.parse::<u8>().ok())
            .map(|q| match q {
                0 => QoS::AtMostOnce,
                2 => QoS::ExactlyOnce,
                _ => QoS::AtLeastOnce,
            })
            .unwrap_or(QoS::AtLeastOnce)
    }

    /// Parse an MQTT URL and build a connected (client, event loop) pair.
    fn build_client(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> Result<(AsyncClient, EventLoop), String> {
        // Parse mqtt://host:port or tcp://host:port
        let url = destination
            .trim_start_matches("mqtt://")
            .trim_start_matches("tcp://");

        let (host, port) = if let Some((h, p)) = url.rsplit_once(':') {
            let port = p.parse::<u16>().map_err(|_| format!("invalid port in URL: {destination}"))?;
            (h.to_string(), port)
        } else {
            (url.to_string(), 1883)
        };

        // Client ID: use property or generate a unique one.
        let client_id = properties
            .get("client_id")
            .filter(|s| !s.is_empty())
            .cloned()
            .unwrap_or_else(|| format!("arca-{}", uuid::Uuid::new_v4()));

        let mut options = MqttOptions::new(client_id, &host, port);
        options.set_keep_alive(Duration::from_secs(5));

        // Optional username/password auth.
        if let Some(username) = properties.get("username").filter(|u| !u.is_empty()) {
            let password = properties.get("password").cloned().unwrap_or_default();
            options.set_credentials(username, password);
        }

        let (client, eventloop) = AsyncClient::new(options, 10);
        Ok((client, eventloop))
    }
}

#[async_trait::async_trait]
impl NotificationConnector for MqttConnector {
    fn name(&self) -> &str {
        "mqtt"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let topic = Self::topic_name(properties).to_owned();
        let qos = Self::qos_level(properties);

        let (client, mut eventloop) = match self.build_client(destination, properties) {
            Ok(pair) => pair,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        let payload_bytes = bytes::Bytes::from(payload.to_owned());
        let topic_clone = topic.clone();

        let result = tokio::time::timeout(self.timeout, async {
            // Wait for ConnAck before publishing (ensures the broker is reachable).
            loop {
                match eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => break,
                    Ok(_) => {}
                    Err(e) => return Err(e.to_string()),
                }
            }

            // Publish the message.
            if let Err(e) = client
                .publish(topic_clone, qos, false, payload_bytes)
                .await
            {
                return Err(e.to_string());
            }

            // Drive the event loop to process the publish and any QoS handshake.
            loop {
                match eventloop.poll().await {
                    Ok(rumqttc::Event::Outgoing(rumqttc::Outgoing::Publish(_))) => break,
                    Ok(_) => {}
                    Err(e) => return Err(e.to_string()),
                }
            }

            let _ = client.disconnect().await;
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => DeliveryResult {
                success: true,
                status_info: format!("PUBLISH to '{topic}' (QoS {qos:?})"),
                error: None,
            },
            Ok(Err(e)) => DeliveryResult {
                success: false,
                status_info: "publish error".to_string(),
                error: Some(e),
            },
            Err(_) => DeliveryResult {
                success: false,
                status_info: "connection timeout".to_string(),
                error: Some("operation timed out".to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let (client, mut eventloop) = match self.build_client(destination, properties) {
            Ok(pair) => pair,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        let result = tokio::time::timeout(self.timeout, async {
            // Poll until we get a ConnAck (successful connection).
            loop {
                match eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Packet::ConnAck(_))) => {
                        let _ = client.disconnect().await;
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(e) => return Err(e.to_string()),
                }
            }
        })
        .await;

        match result {
            Ok(Ok(())) => TestResult {
                success: true,
                status_info: "OK".to_string(),
                error: None,
            },
            Ok(Err(e)) => TestResult {
                success: false,
                status_info: "connection error".to_string(),
                error: Some(e),
            },
            Err(_) => TestResult {
                success: false,
                status_info: "connection timeout".to_string(),
                error: Some("operation timed out".to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_topic_name_from_properties() {
        let mut props = HashMap::new();
        assert_eq!(MqttConnector::topic_name(&props), "arca/notifications");

        props.insert("topic".to_string(), "my/topic".to_string());
        assert_eq!(MqttConnector::topic_name(&props), "my/topic");

        props.insert("topic".to_string(), "".to_string());
        assert_eq!(MqttConnector::topic_name(&props), "arca/notifications");
    }

    #[test]
    fn test_qos_level_from_properties() {
        let mut props = HashMap::new();
        assert_eq!(MqttConnector::qos_level(&props), QoS::AtLeastOnce);

        props.insert("qos".to_string(), "0".to_string());
        assert_eq!(MqttConnector::qos_level(&props), QoS::AtMostOnce);

        props.insert("qos".to_string(), "1".to_string());
        assert_eq!(MqttConnector::qos_level(&props), QoS::AtLeastOnce);

        props.insert("qos".to_string(), "2".to_string());
        assert_eq!(MqttConnector::qos_level(&props), QoS::ExactlyOnce);

        // Invalid values fall back to AtLeastOnce.
        props.insert("qos".to_string(), "abc".to_string());
        assert_eq!(MqttConnector::qos_level(&props), QoS::AtLeastOnce);
    }

    #[test]
    fn test_connector_name() {
        let connector = MqttConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "mqtt");
    }

    #[tokio::test]
    async fn test_deliver_connection_refused() {
        let connector = MqttConnector::new(Duration::from_secs(2));
        let props = HashMap::new();
        let result = connector
            .deliver("mqtt://127.0.0.1:1", "{}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_test_connection_refused() {
        let connector = MqttConnector::new(Duration::from_secs(2));
        let props = HashMap::new();
        let result = connector.test("mqtt://127.0.0.1:1", &props).await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
