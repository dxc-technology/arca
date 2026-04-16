//! Elasticsearch notification connector.
//!
//! Delivers S3 event notifications by indexing JSON documents into
//! Elasticsearch via the REST API. Uses `reqwest` (already a workspace
//! dependency) so no new crates are needed.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};

use super::db_common;

/// Elasticsearch connector — delivers events as indexed documents.
pub struct ElasticsearchConnector {
    client: reqwest::Client,
}

impl ElasticsearchConnector {
    /// Create a new Elasticsearch connector with the given timeout.
    pub fn new(timeout: Duration) -> Self {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .expect("failed to build reqwest client");
        ElasticsearchConnector { client }
    }

    /// Apply optional basic auth from properties.
    fn apply_auth(
        req: reqwest::RequestBuilder,
        properties: &HashMap<String, String>,
    ) -> reqwest::RequestBuilder {
        match (
            properties.get("username").filter(|s| !s.is_empty()),
            properties.get("password").filter(|s| !s.is_empty()),
        ) {
            (Some(user), Some(pass)) => req.basic_auth(user, Some(pass)),
            (Some(user), None) => req.basic_auth(user, Option::<&str>::None),
            _ => req,
        }
    }
}

#[async_trait::async_trait]
impl NotificationConnector for ElasticsearchConnector {
    fn name(&self) -> &str {
        "elasticsearch"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let index = db_common::index_name(properties);

        let fields = match db_common::extract_event_fields(payload) {
            Ok(f) => f,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "payload parse error".to_string(),
                    error: Some(e),
                };
            }
        };

        let doc = serde_json::json!({
            "id": fields.id,
            "event_name": fields.event_name,
            "bucket": fields.bucket,
            "key": fields.key,
            "event_time": fields.event_time.to_rfc3339(),
            "payload": payload,
            "created_at": chrono::Utc::now().to_rfc3339(),
        });

        let url = format!(
            "{}/{}/_doc",
            destination.trim_end_matches('/'),
            index,
        );

        let req = self.client.post(&url).json(&doc);
        let req = Self::apply_auth(req, properties);

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() || status.as_u16() == 201 {
                    DeliveryResult {
                        success: true,
                        status_info: format!("indexed into '{index}' ({status})"),
                        error: None,
                    }
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    DeliveryResult {
                        success: false,
                        status_info: format!("HTTP {status}"),
                        error: Some(body),
                    }
                }
            }
            Err(e) => DeliveryResult {
                success: false,
                status_info: "connection error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let url = format!(
            "{}/_cluster/health",
            destination.trim_end_matches('/'),
        );

        let req = self.client.get(&url);
        let req = Self::apply_auth(req, properties);

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    TestResult {
                        success: true,
                        status_info: format!("cluster health OK ({status}): {body}"),
                        error: None,
                    }
                } else {
                    let body = resp.text().await.unwrap_or_default();
                    TestResult {
                        success: false,
                        status_info: format!("HTTP {status}"),
                        error: Some(body),
                    }
                }
            }
            Err(e) => TestResult {
                success: false,
                status_info: "connection error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connector_name() {
        let connector = ElasticsearchConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "elasticsearch");
    }

    #[tokio::test]
    async fn test_deliver_connection_refused() {
        let connector = ElasticsearchConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("http://127.0.0.1:1", "{\"Records\":[{\"eventName\":\"s3:ObjectCreated:Put\",\"eventTime\":\"2026-01-01T00:00:00Z\",\"s3\":{\"bucket\":{\"name\":\"b\"},\"object\":{\"key\":\"k\"}}}]}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_test_connection_refused() {
        let connector = ElasticsearchConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector.test("http://127.0.0.1:1", &props).await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_deliver_invalid_payload() {
        let connector = ElasticsearchConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("http://127.0.0.1:1", "not json", &props)
            .await;
        assert!(!result.success);
        assert_eq!(result.status_info, "payload parse error");
    }
}
