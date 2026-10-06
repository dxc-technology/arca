//! Webhook notification connector.
//!
//! Delivers S3 event notifications via HTTP POST to a configured URL.
//! Supports optional Bearer token authentication via the `auth_token` property.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};

/// Webhook connector — delivers events as JSON via HTTP POST.
pub struct WebhookConnector {
    /// `Err` when the client could not be built (see `build_http_client`).
    client: Result<reqwest::Client, String>,
}

impl WebhookConnector {
    /// Create a new webhook connector with the given HTTP timeout.
    pub fn new(timeout: Duration) -> Self {
        crate::crypto::ensure_default_crypto_provider();
        Self::from_client(super::build_http_client(
            "webhook",
            reqwest::Client::builder().timeout(timeout),
        ))
    }

    pub(crate) fn from_client(client: Result<reqwest::Client, String>) -> Self {
        WebhookConnector { client }
    }

    /// Build request headers, including optional Bearer auth token.
    fn build_request(
        &self,
        url: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> Result<reqwest::RequestBuilder, String> {
        let mut req = super::usable_client(&self.client)?
            .post(url)
            .header("Content-Type", "application/json")
            .body(payload.to_string());

        if let Some(token) = properties.get("auth_token") {
            if !token.is_empty() {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
        }

        Ok(req)
    }
}

#[async_trait::async_trait]
impl NotificationConnector for WebhookConnector {
    fn name(&self) -> &str {
        "webhook"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let req = match self.build_request(destination, payload, properties) {
            Ok(req) => req,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "error".to_string(),
                    error: Some(e),
                }
            }
        };

        match req.send().await {
            Ok(resp) if resp.status().is_success() => DeliveryResult {
                success: true,
                status_info: format!("HTTP {}", resp.status().as_u16()),
                error: None,
            },
            Ok(resp) => {
                let status = resp.status();
                DeliveryResult {
                    success: false,
                    status_info: format!("HTTP {}", status.as_u16()),
                    error: Some(format!("HTTP {status}")),
                }
            }
            Err(e) => DeliveryResult {
                success: false,
                status_info: "error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let test_event = serde_json::json!({
            "Records": [{
                "eventVersion": "2.1",
                "eventSource": "arca:s3",
                "awsRegion": "test",
                "eventTime": chrono::Utc::now().to_rfc3339(),
                "eventName": "s3:TestEvent",
                "userIdentity": { "principalId": "test" },
                "requestParameters": { "sourceIPAddress": "127.0.0.1" },
                "responseElements": { "x-amz-request-id": "test", "x-amz-id-2": "" },
                "s3": {
                    "s3SchemaVersion": "1.0",
                    "configurationId": "test",
                    "bucket": { "name": "test-bucket", "ownerIdentity": { "principalId": "test" }, "arn": "arn:arca:s3:::test-bucket" },
                    "object": { "key": "test-key", "size": 0, "eTag": "", "sequencer": "000" }
                }
            }]
        });

        let payload = serde_json::to_string(&test_event).unwrap_or_default();
        let req = match self.build_request(destination, &payload, properties) {
            Ok(req) => req,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "error".to_string(),
                    error: Some(e),
                }
            }
        };

        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let success = resp.status().is_success();
                TestResult {
                    success,
                    status_info: format!("HTTP {status}"),
                    error: if success {
                        None
                    } else {
                        Some(format!("HTTP {status}"))
                    },
                }
            }
            Err(e) => TestResult {
                success: false,
                status_info: "error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TD-049: without a CA bundle the client cannot be built; the connector
    // must report it on use instead of aborting the server at startup.
    #[tokio::test]
    async fn unavailable_client_fails_delivery_and_test_with_the_cause() {
        let connector = WebhookConnector::from_client(Err("no CA certificates".to_string()));
        let props = HashMap::new();
        let delivered = connector.deliver("https://hook.example/", "{}", &props).await;
        assert!(!delivered.success);
        let error = delivered.error.unwrap();
        assert!(error.contains("HTTP client unavailable"), "{error}");
        assert!(error.contains("no CA certificates"), "{error}");

        let tested = connector.test("https://hook.example/", &props).await;
        assert!(!tested.success);
        assert!(tested.error.unwrap().contains("no CA certificates"));
    }

    #[tokio::test]
    async fn working_client_still_reaches_the_network() {
        let connector = WebhookConnector::new(Duration::from_secs(1));
        let result = connector.deliver("http://127.0.0.1:1/", "{}", &HashMap::new()).await;
        assert!(!result.success);
        assert!(!result.error.unwrap().contains("HTTP client unavailable"));
    }
}
