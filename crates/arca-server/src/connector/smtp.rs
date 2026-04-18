//! SMTP notification connector.
//!
//! Delivers S3 event notifications as email messages via SMTP or SMTPS.
//! Destination URL format: `smtp://host:port` (plain, optional STARTTLS) or
//! `smtps://host:port` (implicit TLS, typically port 465). Default ports
//! are 25 for SMTP and 465 for SMTPS when the URL does not specify one.
//!
//! Properties:
//!   - `to` (required) — recipient e-mail address
//!   - `from` (optional, default `arca@localhost`) — sender address
//!   - `username` / `password` (optional) — PLAIN authentication
//!   - `subject` (optional, default `Arca S3 Notification: <event>`)
//!   - `starttls` (optional, `true`/`false`) — force STARTTLS on `smtp://`

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};

/// SMTP connector — delivers events as e-mail messages.
pub struct SmtpConnector {
    timeout: Duration,
}

/// SMTP transport scheme parsed from the destination URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SmtpScheme {
    /// Plain SMTP (port 25 by default).
    Plain,
    /// Implicit TLS / SMTPS (port 465 by default).
    Smtps,
}

impl SmtpConnector {
    /// Create a new SMTP connector with the given operation timeout.
    pub fn new(timeout: Duration) -> Self {
        SmtpConnector { timeout }
    }

    /// Parse the destination into (scheme, host, port).
    fn parse_destination(destination: &str) -> Result<(SmtpScheme, String, u16), String> {
        let (scheme, rest) = if let Some(stripped) = destination.strip_prefix("smtps://") {
            (SmtpScheme::Smtps, stripped)
        } else if let Some(stripped) = destination.strip_prefix("smtp://") {
            (SmtpScheme::Plain, stripped)
        } else {
            return Err(format!(
                "invalid SMTP destination (expected smtp:// or smtps:// prefix): {destination}"
            ));
        };

        let (host, port) = if let Some((h, p)) = rest.rsplit_once(':') {
            let parsed: u16 = p
                .parse()
                .map_err(|_| format!("invalid port in SMTP destination: {p}"))?;
            (h.to_string(), parsed)
        } else {
            let default_port = match scheme {
                SmtpScheme::Plain => 25,
                SmtpScheme::Smtps => 465,
            };
            (rest.to_string(), default_port)
        };

        if host.is_empty() {
            return Err(format!("empty host in SMTP destination: {destination}"));
        }

        Ok((scheme, host, port))
    }

    /// Fetch a required string property, returning an error message when absent or empty.
    fn require(properties: &HashMap<String, String>, key: &str) -> Result<String, String> {
        match properties.get(key).map(|s| s.as_str()).filter(|s| !s.is_empty()) {
            Some(v) => Ok(v.to_string()),
            None => Err(format!("missing required property: {key}")),
        }
    }

    /// Fetch an optional string property with a fallback default.
    fn opt<'a>(properties: &'a HashMap<String, String>, key: &str, default: &'a str) -> &'a str {
        properties
            .get(key)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
    }

    /// Returns true when the plain-SMTP `starttls` property is set to `true`.
    fn wants_starttls(properties: &HashMap<String, String>) -> bool {
        matches!(
            properties
                .get("starttls")
                .map(|s| s.trim().to_lowercase())
                .as_deref(),
            Some("true") | Some("yes") | Some("1")
        )
    }

    /// Derive a subject from the payload (best-effort JSON decode for nicer defaults).
    fn default_subject(payload: &str) -> String {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) {
            if let Some(event) = value
                .get("Records")
                .and_then(|r| r.get(0))
                .and_then(|r| r.get("eventName"))
                .and_then(|s| s.as_str())
            {
                return format!("Arca S3 Notification: {event}");
            }
        }
        "Arca S3 Notification".to_string()
    }

    /// Build the e-mail message for the given event payload.
    fn build_message(
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> Result<Message, String> {
        let to_addr = Self::require(properties, "to")?;
        let from_addr = Self::opt(properties, "from", "arca@localhost").to_string();
        let subject = match properties.get("subject").map(|s| s.as_str()).filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => Self::default_subject(payload),
        };

        Message::builder()
            .from(from_addr.parse().map_err(|e| format!("invalid from address: {e}"))?)
            .to(to_addr.parse().map_err(|e| format!("invalid to address: {e}"))?)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(payload.to_string())
            .map_err(|e| format!("failed to build e-mail message: {e}"))
    }

    /// Build the async SMTP transport for the given destination.
    fn build_transport(
        scheme: SmtpScheme,
        host: &str,
        port: u16,
        starttls: bool,
        credentials: Option<Credentials>,
        timeout: Duration,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, String> {
        let builder = match scheme {
            SmtpScheme::Smtps => AsyncSmtpTransport::<Tokio1Executor>::relay(host)
                .map_err(|e| format!("failed to configure SMTPS relay: {e}"))?,
            SmtpScheme::Plain if starttls => {
                AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
                    .map_err(|e| format!("failed to configure STARTTLS relay: {e}"))?
            }
            SmtpScheme::Plain => AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host),
        };

        let builder = builder.port(port).timeout(Some(timeout));
        let builder = match credentials {
            Some(c) => builder.credentials(c),
            None => builder,
        };

        Ok(builder.build())
    }
}

#[async_trait::async_trait]
impl NotificationConnector for SmtpConnector {
    fn name(&self) -> &str {
        "smtp"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let (scheme, host, port) = match Self::parse_destination(destination) {
            Ok(v) => v,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "invalid destination".to_string(),
                    error: Some(e),
                };
            }
        };

        let message = match Self::build_message(payload, properties) {
            Ok(m) => m,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "invalid message".to_string(),
                    error: Some(e),
                };
            }
        };

        let credentials = match (
            properties.get("username").filter(|s| !s.is_empty()).cloned(),
            properties.get("password").filter(|s| !s.is_empty()).cloned(),
        ) {
            (Some(u), Some(p)) => Some(Credentials::new(u, p)),
            _ => None,
        };

        let transport = match Self::build_transport(
            scheme,
            &host,
            port,
            Self::wants_starttls(properties),
            credentials,
            self.timeout,
        ) {
            Ok(t) => t,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "transport error".to_string(),
                    error: Some(e),
                };
            }
        };

        match transport.send(message).await {
            Ok(resp) => DeliveryResult {
                success: resp.is_positive(),
                status_info: format!("SMTP {host}:{port} ({:?})", resp.code()),
                error: if resp.is_positive() {
                    None
                } else {
                    Some(format!("SMTP negative response: {:?}", resp.code()))
                },
            },
            Err(e) => DeliveryResult {
                success: false,
                status_info: "send error".to_string(),
                error: Some(e.to_string()),
            },
        }
    }

    async fn test(
        &self,
        destination: &str,
        _properties: &HashMap<String, String>,
    ) -> TestResult {
        let (_, host, port) = match Self::parse_destination(destination) {
            Ok(v) => v,
            Err(e) => {
                return TestResult {
                    success: false,
                    status_info: "invalid destination".to_string(),
                    error: Some(e),
                };
            }
        };

        let addr = format!("{host}:{port}");

        match tokio::time::timeout(
            self.timeout,
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(_)) => TestResult {
                success: true,
                status_info: format!("TCP connection to {addr} OK"),
                error: None,
            },
            Ok(Err(e)) => TestResult {
                success: false,
                status_info: "connection error".to_string(),
                error: Some(e.to_string()),
            },
            Err(_) => TestResult {
                success: false,
                status_info: "connection timeout".to_string(),
                error: Some("SMTP connect timed out".to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_smtp_plain() {
        let (scheme, host, port) =
            SmtpConnector::parse_destination("smtp://mail.example.com:25").unwrap();
        assert_eq!(scheme, SmtpScheme::Plain);
        assert_eq!(host, "mail.example.com");
        assert_eq!(port, 25);
    }

    #[test]
    fn parse_smtps() {
        let (scheme, host, port) =
            SmtpConnector::parse_destination("smtps://mail.example.com:465").unwrap();
        assert_eq!(scheme, SmtpScheme::Smtps);
        assert_eq!(host, "mail.example.com");
        assert_eq!(port, 465);
    }

    #[test]
    fn parse_defaults_plain_port() {
        let (scheme, host, port) =
            SmtpConnector::parse_destination("smtp://mail.example.com").unwrap();
        assert_eq!(scheme, SmtpScheme::Plain);
        assert_eq!(host, "mail.example.com");
        assert_eq!(port, 25);
    }

    #[test]
    fn parse_defaults_smtps_port() {
        let (scheme, host, port) =
            SmtpConnector::parse_destination("smtps://mail.example.com").unwrap();
        assert_eq!(scheme, SmtpScheme::Smtps);
        assert_eq!(host, "mail.example.com");
        assert_eq!(port, 465);
    }

    #[test]
    fn parse_rejects_missing_scheme() {
        assert!(SmtpConnector::parse_destination("mail.example.com:25").is_err());
    }

    #[test]
    fn parse_rejects_bad_port() {
        assert!(SmtpConnector::parse_destination("smtp://mail.example.com:abc").is_err());
    }

    #[test]
    fn parse_rejects_empty_host() {
        assert!(SmtpConnector::parse_destination("smtp://:25").is_err());
    }

    #[test]
    fn wants_starttls_detects_truthy_values() {
        let mut props = HashMap::new();
        assert!(!SmtpConnector::wants_starttls(&props));
        props.insert("starttls".to_string(), "true".to_string());
        assert!(SmtpConnector::wants_starttls(&props));
        props.insert("starttls".to_string(), "YES".to_string());
        assert!(SmtpConnector::wants_starttls(&props));
        props.insert("starttls".to_string(), "1".to_string());
        assert!(SmtpConnector::wants_starttls(&props));
        props.insert("starttls".to_string(), "no".to_string());
        assert!(!SmtpConnector::wants_starttls(&props));
    }

    #[test]
    fn default_subject_uses_event_name() {
        let payload = r#"{"Records":[{"eventName":"s3:ObjectCreated:Put"}]}"#;
        assert_eq!(
            SmtpConnector::default_subject(payload),
            "Arca S3 Notification: s3:ObjectCreated:Put"
        );
    }

    #[test]
    fn default_subject_fallback_on_bad_json() {
        assert_eq!(
            SmtpConnector::default_subject("not json"),
            "Arca S3 Notification"
        );
    }

    #[test]
    fn build_message_requires_to() {
        let props = HashMap::new();
        let err =
            SmtpConnector::build_message(r#"{"Records":[]}"#, &props).expect_err("must fail");
        assert!(err.contains("to"));
    }

    #[test]
    fn build_message_success_with_defaults() {
        let mut props = HashMap::new();
        props.insert("to".to_string(), "ops@example.com".to_string());
        let msg = SmtpConnector::build_message(r#"{"Records":[]}"#, &props).unwrap();
        // headers contain a From and To and Subject
        let raw = String::from_utf8(msg.formatted()).unwrap();
        assert!(raw.contains("To: ops@example.com"));
        assert!(raw.contains("From: arca@localhost"));
        assert!(raw.contains("Subject: Arca S3 Notification"));
    }

    #[test]
    fn connector_name() {
        let c = SmtpConnector::new(Duration::from_secs(5));
        assert_eq!(c.name(), "smtp");
    }

    #[tokio::test]
    async fn deliver_invalid_destination() {
        let c = SmtpConnector::new(Duration::from_secs(1));
        let mut props = HashMap::new();
        props.insert("to".to_string(), "ops@example.com".to_string());
        let r = c.deliver("not-a-url", r#"{"Records":[]}"#, &props).await;
        assert!(!r.success);
        assert_eq!(r.status_info, "invalid destination");
    }

    #[tokio::test]
    async fn deliver_connection_refused() {
        let c = SmtpConnector::new(Duration::from_secs(1));
        let mut props = HashMap::new();
        props.insert("to".to_string(), "ops@example.com".to_string());
        let r = c
            .deliver("smtp://127.0.0.1:1", r#"{"Records":[]}"#, &props)
            .await;
        assert!(!r.success);
        assert!(r.error.is_some());
    }

    #[tokio::test]
    async fn test_connection_refused() {
        let c = SmtpConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let r = c.test("smtp://127.0.0.1:1", &props).await;
        assert!(!r.success);
        assert!(r.error.is_some());
    }
}
