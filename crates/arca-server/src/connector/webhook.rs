//! Webhook notification connector.
//!
//! Delivers S3 event notifications via HTTP POST to a configured URL.
//! Supports optional Bearer token authentication via the `auth_token` property.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};

/// Webhook connector — delivers events as JSON via HTTP POST.
pub struct WebhookConnector {
    client: reqwest::Client,
}

impl WebhookConnector {
    /// Create a new webhook connector with the given HTTP timeout.
    pub fn new(timeout: Duration) -> Self {
        crate::crypto::ensure_default_crypto_provider();
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .expect("build reqwest client for webhook connector");
        WebhookConnector { client }
    }

    /// Build request headers, including optional Bearer auth token.
    fn build_request(
        &self,
        url: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body(payload.to_string());

        if let Some(token) = properties.get("auth_token") {
            if !token.is_empty() {
                req = req.header("Authorization", format!("Bearer {token}"));
            }
        }

        req
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
        let req = self.build_request(destination, payload, properties);

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
        let req = self.build_request(destination, &payload, properties);

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
