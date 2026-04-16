//! Syslog (RFC 5424) notification connector.
//!
//! Delivers S3 event notifications as RFC 5424 syslog messages over UDP or TCP.
//! The destination URL format is `udp://host:port` or `tcp://host:port`.

use std::collections::HashMap;
use std::time::Duration;

use arca_core::store::connector::{DeliveryResult, NotificationConnector, TestResult};

/// Syslog connector — delivers events as RFC 5424 messages via UDP or TCP.
pub struct SyslogConnector {
    timeout: Duration,
}

impl SyslogConnector {
    /// Create a new Syslog connector with the given timeout.
    pub fn new(timeout: Duration) -> Self {
        SyslogConnector { timeout }
    }

    /// Extract the syslog facility from properties, defaulting to "local0" (16).
    fn facility_code(properties: &HashMap<String, String>) -> u8 {
        let name = properties
            .get("facility")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("local0");

        match name.to_lowercase().as_str() {
            "kern" => 0,
            "user" => 1,
            "mail" => 2,
            "daemon" => 3,
            "auth" => 4,
            "syslog" => 5,
            "lpr" => 6,
            "news" => 7,
            "uucp" => 8,
            "cron" => 9,
            "authpriv" => 10,
            "ftp" => 11,
            "local0" => 16,
            "local1" => 17,
            "local2" => 18,
            "local3" => 19,
            "local4" => 20,
            "local5" => 21,
            "local6" => 22,
            "local7" => 23,
            _ => 16, // default to local0
        }
    }

    /// Extract the syslog severity from properties, defaulting to "informational" (6).
    fn severity_code(properties: &HashMap<String, String>) -> u8 {
        let name = properties
            .get("severity")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("informational");

        match name.to_lowercase().as_str() {
            "emergency" | "emerg" => 0,
            "alert" => 1,
            "critical" | "crit" => 2,
            "error" | "err" => 3,
            "warning" | "warn" => 4,
            "notice" => 5,
            "informational" | "info" => 6,
            "debug" => 7,
            _ => 6, // default to informational
        }
    }

    /// Extract the app name from properties, defaulting to "arca".
    fn app_name(properties: &HashMap<String, String>) -> &str {
        properties
            .get("app_name")
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("arca")
    }

    /// Parse destination URL into (protocol, host, port).
    /// Supports `udp://host:port`, `tcp://host:port`, or `host:port` (defaults to UDP).
    fn parse_destination(destination: &str) -> Result<(&str, &str, u16), String> {
        let (protocol, rest) = if let Some(stripped) = destination.strip_prefix("udp://") {
            ("udp", stripped)
        } else if let Some(stripped) = destination.strip_prefix("tcp://") {
            ("tcp", stripped)
        } else {
            ("udp", destination)
        };

        let (host, port_str) = rest
            .rsplit_once(':')
            .ok_or_else(|| format!("invalid syslog destination (missing port): {destination}"))?;

        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("invalid port in syslog destination: {port_str}"))?;

        if host.is_empty() {
            return Err(format!("empty host in syslog destination: {destination}"));
        }

        Ok((protocol, host, port))
    }

    /// Build an RFC 5424 syslog message.
    fn build_message(
        facility: u8,
        severity: u8,
        app_name: &str,
        payload: &str,
    ) -> String {
        let pri = facility * 8 + severity;
        let timestamp = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // RFC 5424: <PRI>VERSION SP TIMESTAMP SP HOSTNAME SP APP-NAME SP PROCID SP MSGID SP SD SP MSG
        format!("<{pri}>1 {timestamp} arca {app_name} - s3-event - {payload}")
    }
}

#[async_trait::async_trait]
impl NotificationConnector for SyslogConnector {
    fn name(&self) -> &str {
        "syslog"
    }

    async fn deliver(
        &self,
        destination: &str,
        payload: &str,
        properties: &HashMap<String, String>,
    ) -> DeliveryResult {
        let (protocol, host, port) = match Self::parse_destination(destination) {
            Ok(v) => v,
            Err(e) => {
                return DeliveryResult {
                    success: false,
                    status_info: "invalid destination".to_string(),
                    error: Some(e),
                };
            }
        };

        let facility = Self::facility_code(properties);
        let severity = Self::severity_code(properties);
        let app_name = Self::app_name(properties);
        let message = Self::build_message(facility, severity, app_name, payload);

        let addr = format!("{host}:{port}");

        match protocol {
            "tcp" => {
                let stream = match tokio::time::timeout(
                    self.timeout,
                    tokio::net::TcpStream::connect(&addr),
                )
                .await
                {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        return DeliveryResult {
                            success: false,
                            status_info: "connection error".to_string(),
                            error: Some(e.to_string()),
                        };
                    }
                    Err(_) => {
                        return DeliveryResult {
                            success: false,
                            status_info: "connection timeout".to_string(),
                            error: Some("TCP connect timed out".to_string()),
                        };
                    }
                };

                use tokio::io::AsyncWriteExt;
                // RFC 5424 over TCP uses newline-delimited framing (octet counting is optional)
                let framed = format!("{message}\n");
                match tokio::time::timeout(
                    self.timeout,
                    async {
                        let (_, mut writer) = stream.into_split();
                        writer.write_all(framed.as_bytes()).await
                    },
                )
                .await
                {
                    Ok(Ok(())) => DeliveryResult {
                        success: true,
                        status_info: format!("TCP to {addr}"),
                        error: None,
                    },
                    Ok(Err(e)) => DeliveryResult {
                        success: false,
                        status_info: "write error".to_string(),
                        error: Some(e.to_string()),
                    },
                    Err(_) => DeliveryResult {
                        success: false,
                        status_info: "write timeout".to_string(),
                        error: Some("TCP write timed out".to_string()),
                    },
                }
            }
            _ => {
                // UDP (default)
                let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => s,
                    Err(e) => {
                        return DeliveryResult {
                            success: false,
                            status_info: "socket error".to_string(),
                            error: Some(e.to_string()),
                        };
                    }
                };

                match tokio::time::timeout(
                    self.timeout,
                    socket.send_to(message.as_bytes(), &addr),
                )
                .await
                {
                    Ok(Ok(n)) => DeliveryResult {
                        success: true,
                        status_info: format!("UDP to {addr} ({n} bytes)"),
                        error: None,
                    },
                    Ok(Err(e)) => DeliveryResult {
                        success: false,
                        status_info: "send error".to_string(),
                        error: Some(e.to_string()),
                    },
                    Err(_) => DeliveryResult {
                        success: false,
                        status_info: "send timeout".to_string(),
                        error: Some("UDP send timed out".to_string()),
                    },
                }
            }
        }
    }

    async fn test(
        &self,
        destination: &str,
        properties: &HashMap<String, String>,
    ) -> TestResult {
        let (protocol, host, port) = match Self::parse_destination(destination) {
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

        match protocol {
            "tcp" => {
                // TCP: test by connecting
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
                        error: Some("TCP connect timed out".to_string()),
                    },
                }
            }
            _ => {
                // UDP: send a test message (fire-and-forget)
                let app_name = Self::app_name(properties);
                let facility = Self::facility_code(properties);
                let severity = Self::severity_code(properties);
                let test_msg = Self::build_message(
                    facility,
                    severity,
                    app_name,
                    r#"{"test":true}"#,
                );

                let socket = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
                    Ok(s) => s,
                    Err(e) => {
                        return TestResult {
                            success: false,
                            status_info: "socket error".to_string(),
                            error: Some(e.to_string()),
                        };
                    }
                };

                match tokio::time::timeout(
                    self.timeout,
                    socket.send_to(test_msg.as_bytes(), &addr),
                )
                .await
                {
                    Ok(Ok(_)) => TestResult {
                        success: true,
                        status_info: format!("UDP test message sent to {addr}"),
                        error: None,
                    },
                    Ok(Err(e)) => TestResult {
                        success: false,
                        status_info: "send error".to_string(),
                        error: Some(e.to_string()),
                    },
                    Err(_) => TestResult {
                        success: false,
                        status_info: "send timeout".to_string(),
                        error: Some("UDP send timed out".to_string()),
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_facility_code_defaults() {
        let props = HashMap::new();
        assert_eq!(SyslogConnector::facility_code(&props), 16); // local0
    }

    #[test]
    fn test_facility_code_custom() {
        let mut props = HashMap::new();
        props.insert("facility".to_string(), "daemon".to_string());
        assert_eq!(SyslogConnector::facility_code(&props), 3);

        props.insert("facility".to_string(), "local7".to_string());
        assert_eq!(SyslogConnector::facility_code(&props), 23);
    }

    #[test]
    fn test_severity_code_defaults() {
        let props = HashMap::new();
        assert_eq!(SyslogConnector::severity_code(&props), 6); // informational
    }

    #[test]
    fn test_severity_code_custom() {
        let mut props = HashMap::new();
        props.insert("severity".to_string(), "error".to_string());
        assert_eq!(SyslogConnector::severity_code(&props), 3);

        props.insert("severity".to_string(), "debug".to_string());
        assert_eq!(SyslogConnector::severity_code(&props), 7);
    }

    #[test]
    fn test_app_name_default() {
        let props = HashMap::new();
        assert_eq!(SyslogConnector::app_name(&props), "arca");
    }

    #[test]
    fn test_app_name_custom() {
        let mut props = HashMap::new();
        props.insert("app_name".to_string(), "my-app".to_string());
        assert_eq!(SyslogConnector::app_name(&props), "my-app");
    }

    #[test]
    fn test_parse_destination_udp() {
        let (proto, host, port) = SyslogConnector::parse_destination("udp://syslog:514").unwrap();
        assert_eq!(proto, "udp");
        assert_eq!(host, "syslog");
        assert_eq!(port, 514);
    }

    #[test]
    fn test_parse_destination_tcp() {
        let (proto, host, port) = SyslogConnector::parse_destination("tcp://syslog:1514").unwrap();
        assert_eq!(proto, "tcp");
        assert_eq!(host, "syslog");
        assert_eq!(port, 1514);
    }

    #[test]
    fn test_parse_destination_bare() {
        let (proto, host, port) = SyslogConnector::parse_destination("syslog:514").unwrap();
        assert_eq!(proto, "udp");
        assert_eq!(host, "syslog");
        assert_eq!(port, 514);
    }

    #[test]
    fn test_parse_destination_invalid() {
        assert!(SyslogConnector::parse_destination("invalid").is_err());
        assert!(SyslogConnector::parse_destination("udp://host:abc").is_err());
        assert!(SyslogConnector::parse_destination("udp://:514").is_err());
    }

    #[test]
    fn test_connector_name() {
        let connector = SyslogConnector::new(Duration::from_secs(5));
        assert_eq!(connector.name(), "syslog");
    }

    #[test]
    fn test_build_message_format() {
        let msg = SyslogConnector::build_message(16, 6, "arca", r#"{"test":true}"#);
        // PRI = 16*8 + 6 = 134
        assert!(msg.starts_with("<134>1 "));
        assert!(msg.contains("arca arca - s3-event - "));
        assert!(msg.ends_with(r#"{"test":true}"#));
    }

    #[tokio::test]
    async fn test_deliver_tcp_connection_refused() {
        let connector = SyslogConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector
            .deliver("tcp://127.0.0.1:1", "{}", &props)
            .await;
        assert!(!result.success);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn test_deliver_invalid_destination() {
        let connector = SyslogConnector::new(Duration::from_secs(1));
        let props = HashMap::new();
        let result = connector.deliver("invalid", "{}", &props).await;
        assert!(!result.success);
        assert_eq!(result.status_info, "invalid destination");
    }
}
