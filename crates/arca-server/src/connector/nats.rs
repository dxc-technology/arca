//! NATS notification connector.
//!
//! Delivers S3 event notifications by publishing JSON payloads to a NATS
//! subject. Supports optional authentication via `token` or `user`/`password`
//! properties.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use bytes::Bytes;

/// NATS connector — delivers events via PUBLISH to a NATS subject.
pub struct NatsConnector {
    timeout: Duration,
}

impl NatsConnector {
    /// Create a new NATS connector with the given connection timeout.
    pub fn new(timeout: Duration) -> Self {
        NatsConnector { timeout }
    }

    /// Extract the subject name from properties, defaulting to "arca.notifications".
    fn subject_name(properties: &HashMap<String, String>) -> &str {
        properties
            .get("subject")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("arca.notifications")
    }

    /// Open an async connection to the given NATS server URL.
    async fn connect(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> Result<async_nats::Client, String> {
        let mut options = async_nats::ConnectOptions::new();

        // Optional token auth.
        if let Some(token) = properties.get("token").filter(|t| !t.is_empty()) {
            options = options.token(token.clone());
        }

        // Optional user/password auth.
        if let Some(user) = properties.get("user").filter(|u| !u.is_empty()) {
            let pass = properties.get("password").cloned().unwrap_or_default();
            options = options.user_and_password(user.clone(), pass);
        }

        tokio::time::timeout(self.timeout, options.connect(destination))
            .await
            .map_err(|_| "connection timeout".to_string())?
            .map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl NotificationConnector for NatsConnector {
    fn name(&self) -> &str {
        "nats"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let subject = Self::subject_name(properties).to_owned();

        let client = match self.connect(destination, properties).await {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        if let Err(e) = client
            .publish(subject.clone(), Bytes::from(payload.to_owned()))
            .await
        {
            return DeliveryResult {
                success: false,
                status_info: "publish error".to_string(),
                error: Some(e.to_string()),
            };
        }

        // Flush to ensure the message is delivered to the server.
        match client.flush().await {
            Ok(()) => DeliveryResult {
                success: true,
                status_info: format!("PUBLISH to '{subject}'"),
                error: None,
            },
            Err(e) => DeliveryResult {
                success: false,
                status_info: "flush error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let client = match self.connect(destination, properties).await {
            Ok(c) => c,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        // flush() confirms the client can talk to the server.
        match client.flush().await {
            Ok(()) => TestResult {
                success: true,
                status_info: "OK".to_string(),
                error: None,
            },
            Err(e) => TestResult {
                success: false,
                status_info: "flush error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_subject_name_from_properties() {
        let mut props = HashMap::new();
        assert_eq!(NatsConnector::subject_name(&props), "arca.notifications");

        props.insert("subject".to_string(), "my.subject".to_string());
        assert_eq!(NatsConnector::subject_name(&props), "my.subject");

        props.insert("subject".to_string(), "".to_string());
        assert_eq!(NatsConnector::subject_name(&props), "arca.notifications");
    }

    #[test]
    fn test_connector_name() {
        let connector = NatsConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "nats");
    }

    #[tokio::test]
    async fn test_deliver_connection_refused() {
        let connector = NatsConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("nats://127.0.0.1:1", "{}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_test_connection_refused() {
        let connector = NatsConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector.test("nats://127.0.0.1:1", &props).await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_invalid_url() {
        let connector = NatsConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("not-a-valid-url", "{}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }
}
