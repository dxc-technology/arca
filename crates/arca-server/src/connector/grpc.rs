//! gRPC notification connector.
//!
//! Delivers S3 event notifications over a unary gRPC call defined by
//! `arca.notifications.v1.NotificationService`. The destination URL selects
//! the transport:
//!   - `http://host:port` — HTTP/2 cleartext (h2c)
//!   - `https://host:port` — HTTP/2 over TLS
//!
//! Properties:
//!   - `auth_token` (optional) — Bearer token forwarded as `authorization`
//!     request metadata.
//!   - `ca_certificate` (optional) — PEM-encoded CA certificate used to
//!     trust servers presenting self-signed certs in TLS mode.
//!   - `domain_name` (optional) — override TLS SNI / domain name.
//!   - any other key/value pairs — forwarded to the server via the proto
//!     `metadata` map.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use tonic::metadata::MetadataValue;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};
use tonic::Request;

#[allow(clippy::derive_partial_eq_without_eq)]
pub mod proto {
    tonic::include_proto!("arca.notifications.v1");
}

use proto::notification_service_client::NotificationServiceClient;
use proto::NotificationRequest;

/// Properties that are handled directly by the connector and therefore should
/// NOT be forwarded to the peer via the proto `metadata` map.
const RESERVED_PROPERTIES: &[&str] = &["auth_token", "ca_certificate", "domain_name", "insecure"];

/// gRPC connector — delivers events via a unary Notify RPC.
pub struct GrpcConnector {
    timeout: Duration,
}

impl GrpcConnector {
    /// Create a new gRPC connector with the given operation timeout.
    pub fn new(timeout: Duration) -> Self {
        GrpcConnector { timeout }
    }

    /// Validate the destination URL has a supported scheme.
    fn validate_destination(destination: &str) -> Result<bool, String> {
        if let Some(rest) = destination.strip_prefix("https://") {
            if rest.is_empty() {
                return Err(format!("empty host in gRPC destination: {destination}"));
            }
            Ok(true)
        } else if let Some(rest) = destination.strip_prefix("http://") {
            if rest.is_empty() {
                return Err(format!("empty host in gRPC destination: {destination}"));
            }
            Ok(false)
        } else {
            Err(format!(
                "invalid gRPC destination (expected http:// or https:// prefix): {destination}"
            ))
        }
    }

    /// Build a connected Tonic channel, honoring TLS and timeout settings.
    async fn build_channel(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> Result<tonic::transport::Channel, String> {
        let is_tls = Self::validate_destination(destination)?;

        let mut endpoint = Endpoint::from_shared(destination.to_string())
            .map_err(|e| format!("invalid endpoint: {e}"))?
            .connect_timeout(self.timeout)
            .timeout(self.timeout);

        if is_tls {
            let mut tls = ClientTlsConfig::new().with_enabled_roots();
            if let Some(ca_pem) = properties
                .get("ca_certificate")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
            {
                tls = tls.ca_certificate(Certificate::from_pem(ca_pem));
            }
            if let Some(domain) = properties
                .get("domain_name")
                .map(|s| s.as_str())
                .filter(|s| !s.is_empty())
            {
                tls = tls.domain_name(domain);
            }
            endpoint = endpoint
                .tls_config(tls)
                .map_err(|e| format!("failed to configure TLS: {e}"))?;
        }

        endpoint
            .connect()
            .await
            .map_err(|e| format!("connection failed: {e}"))
    }

    /// Apply optional Bearer authentication and user metadata to the request.
    fn apply_metadata(
        req: &mut Request<NotificationRequest>,
        properties: &HashMap<String, String>,
    ) -> Result<(), String> {
        if let Some(token) = properties.get("auth_token").filter(|s| !s.is_empty()) {
            let bearer = format!("Bearer {token}");
            let value = MetadataValue::try_from(bearer.as_str())
                .map_err(|e| format!("invalid auth_token: {e}"))?;
            req.metadata_mut().insert("authorization", value);
        }
        Ok(())
    }

    /// Forwarded (proto `metadata`) properties — everything except the reserved keys.
    fn proto_metadata(properties: &HashMap<String, String>) -> HashMap<String, String> {
        properties
            .iter()
            .filter(|(k, _)| !RESERVED_PROPERTIES.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

#[async_trait::async_trait]
impl NotificationConnector for GrpcConnector {
    fn name(&self) -> &str {
        "grpc"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let channel = match self.build_channel(destination, properties).await {
            Ok(c) => c,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        let mut client = NotificationServiceClient::new(channel);

        let request_body = NotificationRequest {
            event_payload: payload.to_string(),
            connector_id: properties
                .get("connector_id")
                .cloned()
                .unwrap_or_default(),
            metadata: Self::proto_metadata(properties),
        };

        let mut req = Request::new(request_body);
        if let Err(e) = Self::apply_metadata(&mut req, properties) {
            return DeliveryResult {
                success: false,
                status_info: "metadata error".to_string(),
                error: Some(e),
            };
        }

        match client.notify(req).await {
            Ok(resp) => {
                let inner = resp.into_inner();
                DeliveryResult {
                    success: inner.success,
                    status_info: if inner.message.is_empty() {
                        "gRPC OK".to_string()
                    } else {
                        inner.message
                    },
                    error: if inner.success {
                        None
                    } else {
                        Some("server reported failure".to_string())
                    },
                }
            }
            Err(status) => DeliveryResult {
                success: false,
                status_info: format!("gRPC {:?}", status.code()),
                error: Some(status.message().to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let channel = match self.build_channel(destination, properties).await {
            Ok(c) => c,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "connection error".to_string(),
                    error: Some(e),
                };
            }
        };

        let mut client = NotificationServiceClient::new(channel);

        let request_body = NotificationRequest {
            event_payload: r#"{"test":true}"#.to_string(),
            connector_id: "test".to_string(),
            metadata: Self::proto_metadata(properties),
        };

        let mut req = Request::new(request_body);
        if let Err(e) = Self::apply_metadata(&mut req, properties) {
            return TestResult {
                success: false,
                status_info: "metadata error".to_string(),
                error: Some(e),
            };
        }

        match client.notify(req).await {
            Ok(_) => TestResult {
                success: true,
                status_info: "gRPC Notify OK".to_string(),
                error: None,
            },
            Err(status) => {
                // Any transport-level success (even Unimplemented on the peer) means the
                // endpoint accepted our gRPC call — so the connectivity itself is good.
                if matches!(
                    status.code(),
                    tonic::Code::Unimplemented | tonic::Code::Unauthenticated
                ) {
                    TestResult {
                        success: true,
                        status_info: format!("gRPC reachable ({:?})", status.code()),
                        error: None,
                    }
                } else {
                    TestResult {
                        success: false,
                        status_info: format!("gRPC {:?}", status.code()),
                        error: Some(status.message().to_string()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_destination_accepts_http() {
        assert_eq!(
            GrpcConnector::validate_destination("http://grpc-receiver:50051").unwrap(),
            false
        );
    }

    #[test]
    fn validate_destination_accepts_https() {
        assert_eq!(
            GrpcConnector::validate_destination("https://grpc-receiver:50051").unwrap(),
            true
        );
    }

    #[test]
    fn validate_destination_rejects_missing_scheme() {
        assert!(GrpcConnector::validate_destination("grpc-receiver:50051").is_err());
    }

    #[test]
    fn validate_destination_rejects_empty_host() {
        assert!(GrpcConnector::validate_destination("http://").is_err());
        assert!(GrpcConnector::validate_destination("https://").is_err());
    }

    #[test]
    fn proto_metadata_filters_reserved_keys() {
        let mut props = HashMap::new();
        props.insert("auth_token".to_string(), "secret".to_string());
        props.insert("ca_certificate".to_string(), "-----BEGIN CERTIFICATE-----".to_string());
        props.insert("domain_name".to_string(), "example.com".to_string());
        props.insert("insecure".to_string(), "true".to_string());
        props.insert("tenant".to_string(), "t1".to_string());
        props.insert("channel".to_string(), "alpha".to_string());

        let meta = GrpcConnector::proto_metadata(&props);
        assert!(!meta.contains_key("auth_token"));
        assert!(!meta.contains_key("ca_certificate"));
        assert!(!meta.contains_key("domain_name"));
        assert!(!meta.contains_key("insecure"));
        assert_eq!(meta.get("tenant"), Some(&"t1".to_string()));
        assert_eq!(meta.get("channel"), Some(&"alpha".to_string()));
    }

    #[test]
    fn apply_metadata_injects_bearer_token() {
        let mut props = HashMap::new();
        props.insert("auth_token".to_string(), "abc123".to_string());
        let mut req = Request::new(NotificationRequest::default());
        GrpcConnector::apply_metadata(&mut req, &props).unwrap();
        let val = req.metadata().get("authorization").unwrap();
        assert_eq!(val.to_str().unwrap(), "Bearer abc123");
    }

    #[test]
    fn apply_metadata_skips_when_token_absent() {
        let props = HashMap::new();
        let mut req = Request::new(NotificationRequest::default());
        GrpcConnector::apply_metadata(&mut req, &props).unwrap();
        assert!(req.metadata().get("authorization").is_none());
    }

    #[test]
    fn connector_name() {
        let c = GrpcConnector::new(Duration::from_secs(5));
        assert_eq!(c.name(), "grpc");
    }

    #[tokio::test]
    async fn deliver_invalid_destination() {
        let c = GrpcConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let r = c.deliver("not-a-url", r#"{}"#, &props).await;
        assert!(!r.success);
        assert!(r.error.is_some());
    }

    #[tokio::test]
    async fn deliver_connection_refused() {
        let c = GrpcConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let r = c.deliver("http://127.0.0.1:1", r#"{}"#, &props).await;
        assert!(!r.success);
        assert!(r.error.is_some());
    }

    #[tokio::test]
    async fn test_connection_refused() {
        let c = GrpcConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let r = c.test("http://127.0.0.1:1", &props).await;
        assert!(!r.success);
        assert!(r.error.is_some());
    }
}
