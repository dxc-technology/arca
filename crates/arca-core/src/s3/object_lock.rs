//! S3 Object Lock types, XML parsing, and serialization.

use chrono::{DateTime, Utc};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, Event};
use quick_xml::Writer;
use serde::{Deserialize, Serialize};

use crate::error::{write_xml_element, S3Error, S3ErrorCode};

// ── Bucket-level Object Lock configuration ──

/// Object Lock configuration stored per-bucket as JSON in bucket_config.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObjectLockConfiguration {
    /// Always true once Object Lock is enabled.
    /// Not read directly, but presence of the config implies enabled.
    #[allow(dead_code)]
    pub enabled: bool,
    /// Default retention applied to new objects that don't specify their own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_retention: Option<DefaultRetention>,
}

/// Default retention settings for a bucket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DefaultRetention {
    /// "GOVERNANCE" or "COMPLIANCE".
    pub mode: String,
    /// Retention period in days (mutually exclusive with years).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    /// Retention period in years (mutually exclusive with days).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub years: Option<u32>,
}

// ── XML parsing ──

/// Parse an S3 ObjectLockConfiguration XML request body.
pub fn parse_object_lock_configuration_xml(
    xml: &str,
) -> Result<ObjectLockConfiguration, S3Error> {
    #[derive(Deserialize)]
    #[serde(rename = "ObjectLockConfiguration")]
    struct XmlConfig {
        #[serde(rename = "ObjectLockEnabled")]
        enabled: Option<String>,
        #[serde(rename = "Rule")]
        rule: Option<XmlRule>,
    }

    #[derive(Deserialize)]
    struct XmlRule {
        #[serde(rename = "DefaultRetention")]
        default_retention: Option<XmlDefaultRetention>,
    }

    #[derive(Deserialize)]
    struct XmlDefaultRetention {
        #[serde(rename = "Mode")]
        mode: Option<String>,
        #[serde(rename = "Days")]
        days: Option<u32>,
        #[serde(rename = "Years")]
        years: Option<u32>,
    }

    let parsed: XmlConfig = quick_xml::de::from_str(xml).map_err(|e| {
        S3Error::with_message(
            S3ErrorCode::MalformedXML,
            format!("Invalid ObjectLockConfiguration XML: {e}"),
            "",
        )
    })?;

    // Validate ObjectLockEnabled value: must be "Enabled" (or absent).
    if let Some(ref enabled) = parsed.enabled {
        if enabled != "Enabled" {
            return Err(S3Error::with_message(
                S3ErrorCode::MalformedXML,
                "ObjectLockEnabled must be 'Enabled'",
                "",
            ));
        }
    }

    let default_retention = if let Some(rule) = parsed.rule {
        if let Some(dr) = rule.default_retention {
            let mode = dr.mode.ok_or_else(|| {
                S3Error::with_message(S3ErrorCode::MalformedXML, "DefaultRetention must have Mode", "")
            })?;
            validate_retention_mode(&mode)?;
            validate_retention_period(dr.days, dr.years)?;
            Some(DefaultRetention {
                mode,
                days: dr.days,
                years: dr.years,
            })
        } else {
            None
        }
    } else {
        None
    };

    Ok(ObjectLockConfiguration {
        enabled: true,
        default_retention,
    })
}

/// Serialize an ObjectLockConfiguration to S3-compatible XML.
pub fn object_lock_configuration_to_xml(config: &ObjectLockConfiguration) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("ObjectLockConfiguration");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer.write_event(Event::Start(root)).expect("write root start");

    write_xml_element(&mut writer, "ObjectLockEnabled", "Enabled");

    if let Some(ref dr) = config.default_retention {
        writer
            .write_event(Event::Start(BytesStart::new("Rule")))
            .expect("write Rule start");
        writer
            .write_event(Event::Start(BytesStart::new("DefaultRetention")))
            .expect("write DefaultRetention start");
        write_xml_element(&mut writer, "Mode", &dr.mode);
        if let Some(days) = dr.days {
            write_xml_element(&mut writer, "Days", &days.to_string());
        }
        if let Some(years) = dr.years {
            write_xml_element(&mut writer, "Years", &years.to_string());
        }
        writer
            .write_event(Event::End(BytesEnd::new("DefaultRetention")))
            .expect("write DefaultRetention end");
        writer
            .write_event(Event::End(BytesEnd::new("Rule")))
            .expect("write Rule end");
    }

    writer
        .write_event(Event::End(BytesEnd::new("ObjectLockConfiguration")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

// ── Per-object retention XML ──

/// Parse an S3 Retention XML body.
/// Returns (mode, retain_until_date_rfc3339).
pub fn parse_retention_xml(xml: &str) -> Result<(String, DateTime<Utc>), S3Error> {
    #[derive(Deserialize)]
    #[serde(rename = "Retention")]
    struct XmlRetention {
        #[serde(rename = "Mode")]
        mode: Option<String>,
        #[serde(rename = "RetainUntilDate")]
        retain_until_date: Option<String>,
    }

    let parsed: XmlRetention = quick_xml::de::from_str(xml).map_err(|e| {
        S3Error::with_message(
            S3ErrorCode::MalformedXML,
            format!("Invalid Retention XML: {e}"),
            "",
        )
    })?;

    let mode = parsed.mode.ok_or_else(|| {
        S3Error::with_message(S3ErrorCode::MalformedXML, "Retention must have Mode", "")
    })?;
    validate_retention_mode(&mode)?;

    let date_str = parsed.retain_until_date.ok_or_else(|| {
        S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "Retention must have RetainUntilDate",
            "",
        )
    })?;
    let date = DateTime::parse_from_rfc3339(&date_str)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| {
            S3Error::with_message(
                S3ErrorCode::InvalidArgument,
                "RetainUntilDate must be a valid ISO 8601 / RFC 3339 timestamp",
                "",
            )
        })?;

    Ok((mode, date))
}

/// Serialize a Retention response to S3-compatible XML.
pub fn retention_to_xml(mode: &str, retain_until_date: &DateTime<Utc>) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("Retention");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer.write_event(Event::Start(root)).expect("write root");

    write_xml_element(&mut writer, "Mode", mode);
    write_xml_element(
        &mut writer,
        "RetainUntilDate",
        &retain_until_date.to_rfc3339(),
    );

    writer
        .write_event(Event::End(BytesEnd::new("Retention")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

// ── Legal hold XML ──

/// Parse an S3 LegalHold XML body. Returns "ON" or "OFF".
pub fn parse_legal_hold_xml(xml: &str) -> Result<String, S3Error> {
    #[derive(Deserialize)]
    #[serde(rename = "LegalHold")]
    struct XmlLegalHold {
        #[serde(rename = "Status")]
        status: Option<String>,
    }

    let parsed: XmlLegalHold = quick_xml::de::from_str(xml).map_err(|e| {
        S3Error::with_message(
            S3ErrorCode::MalformedXML,
            format!("Invalid LegalHold XML: {e}"),
            "",
        )
    })?;

    let status = parsed.status.ok_or_else(|| {
        S3Error::with_message(S3ErrorCode::MalformedXML, "LegalHold must have Status", "")
    })?;

    match status.as_str() {
        "ON" | "OFF" => Ok(status),
        _ => Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "LegalHold Status must be ON or OFF",
            "",
        )),
    }
}

/// Serialize a LegalHold response to S3-compatible XML.
pub fn legal_hold_to_xml(status: &str) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("write XML decl");

    let mut root = BytesStart::new("LegalHold");
    root.push_attribute(("xmlns", "http://s3.amazonaws.com/doc/2006-03-01/"));
    writer.write_event(Event::Start(root)).expect("write root");

    write_xml_element(&mut writer, "Status", status);

    writer
        .write_event(Event::End(BytesEnd::new("LegalHold")))
        .expect("write root end");

    String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
}

// ── Validation helpers ──

fn validate_retention_mode(mode: &str) -> Result<(), S3Error> {
    match mode {
        "GOVERNANCE" | "COMPLIANCE" => Ok(()),
        _ => Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "Retention mode must be GOVERNANCE or COMPLIANCE",
            "",
        )),
    }
}

fn validate_retention_period(days: Option<u32>, years: Option<u32>) -> Result<(), S3Error> {
    match (days, years) {
        (Some(d), None) => {
            if d == 0 {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidRetentionPeriod,
                    "Retention days must be a positive integer",
                    "",
                ));
            }
            Ok(())
        }
        (None, Some(y)) => {
            if y == 0 {
                return Err(S3Error::with_message(
                    S3ErrorCode::InvalidRetentionPeriod,
                    "Retention years must be a positive integer",
                    "",
                ));
            }
            Ok(())
        }
        (Some(_), Some(_)) => Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "DefaultRetention must specify either Days or Years, not both",
            "",
        )),
        (None, None) => Err(S3Error::with_message(
            S3ErrorCode::MalformedXML,
            "DefaultRetention must specify either Days or Years",
            "",
        )),
    }
}

/// Compute the retain-until-date from a DefaultRetention and a base time.
pub fn compute_retain_until(dr: &DefaultRetention, now: DateTime<Utc>) -> DateTime<Utc> {
    if let Some(days) = dr.days {
        now + chrono::Duration::days(days as i64)
    } else if let Some(years) = dr.years {
        now + chrono::Duration::days(years as i64 * 365)
    } else {
        now
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_object_lock_config_with_governance() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ObjectLockConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <ObjectLockEnabled>Enabled</ObjectLockEnabled>
  <Rule>
    <DefaultRetention>
      <Mode>GOVERNANCE</Mode>
      <Days>30</Days>
    </DefaultRetention>
  </Rule>
</ObjectLockConfiguration>"#;

        let config = parse_object_lock_configuration_xml(xml).unwrap();
        assert!(config.enabled);
        let dr = config.default_retention.unwrap();
        assert_eq!(dr.mode, "GOVERNANCE");
        assert_eq!(dr.days, Some(30));
        assert_eq!(dr.years, None);
    }

    #[test]
    fn parse_object_lock_config_with_compliance_years() {
        let xml = r#"<ObjectLockConfiguration>
  <ObjectLockEnabled>Enabled</ObjectLockEnabled>
  <Rule>
    <DefaultRetention>
      <Mode>COMPLIANCE</Mode>
      <Years>1</Years>
    </DefaultRetention>
  </Rule>
</ObjectLockConfiguration>"#;

        let config = parse_object_lock_configuration_xml(xml).unwrap();
        let dr = config.default_retention.unwrap();
        assert_eq!(dr.mode, "COMPLIANCE");
        assert_eq!(dr.years, Some(1));
    }

    #[test]
    fn parse_object_lock_config_no_rule() {
        let xml = r#"<ObjectLockConfiguration>
  <ObjectLockEnabled>Enabled</ObjectLockEnabled>
</ObjectLockConfiguration>"#;

        let config = parse_object_lock_configuration_xml(xml).unwrap();
        assert!(config.enabled);
        assert!(config.default_retention.is_none());
    }

    #[test]
    fn reject_both_days_and_years() {
        let xml = r#"<ObjectLockConfiguration>
  <ObjectLockEnabled>Enabled</ObjectLockEnabled>
  <Rule>
    <DefaultRetention>
      <Mode>GOVERNANCE</Mode>
      <Days>30</Days>
      <Years>1</Years>
    </DefaultRetention>
  </Rule>
</ObjectLockConfiguration>"#;

        let err = parse_object_lock_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn reject_invalid_mode() {
        let xml = r#"<ObjectLockConfiguration>
  <ObjectLockEnabled>Enabled</ObjectLockEnabled>
  <Rule>
    <DefaultRetention>
      <Mode>INVALID</Mode>
      <Days>30</Days>
    </DefaultRetention>
  </Rule>
</ObjectLockConfiguration>"#;

        let err = parse_object_lock_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn reject_zero_days() {
        let xml = r#"<ObjectLockConfiguration>
  <ObjectLockEnabled>Enabled</ObjectLockEnabled>
  <Rule>
    <DefaultRetention>
      <Mode>GOVERNANCE</Mode>
      <Days>0</Days>
    </DefaultRetention>
  </Rule>
</ObjectLockConfiguration>"#;

        let err = parse_object_lock_configuration_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRetentionPeriod);
    }

    #[test]
    fn xml_roundtrip_config() {
        let config = ObjectLockConfiguration {
            enabled: true,
            default_retention: Some(DefaultRetention {
                mode: "COMPLIANCE".to_string(),
                days: Some(90),
                years: None,
            }),
        };
        let xml = object_lock_configuration_to_xml(&config);
        let parsed = parse_object_lock_configuration_xml(&xml).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn xml_roundtrip_config_no_rule() {
        let config = ObjectLockConfiguration {
            enabled: true,
            default_retention: None,
        };
        let xml = object_lock_configuration_to_xml(&config);
        let parsed = parse_object_lock_configuration_xml(&xml).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn json_roundtrip_config() {
        let config = ObjectLockConfiguration {
            enabled: true,
            default_retention: Some(DefaultRetention {
                mode: "GOVERNANCE".to_string(),
                days: None,
                years: Some(2),
            }),
        };
        let json = serde_json::to_string(&config).unwrap();
        let parsed: ObjectLockConfiguration = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn parse_retention_xml_valid() {
        let xml = r#"<Retention>
  <Mode>COMPLIANCE</Mode>
  <RetainUntilDate>2027-01-01T00:00:00Z</RetainUntilDate>
</Retention>"#;

        let (mode, date) = parse_retention_xml(xml).unwrap();
        assert_eq!(mode, "COMPLIANCE");
        assert_eq!(date.year(), 2027);
    }

    #[test]
    fn retention_xml_roundtrip() {
        let date = DateTime::parse_from_rfc3339("2027-06-15T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let xml = retention_to_xml("GOVERNANCE", &date);
        let (mode, parsed_date) = parse_retention_xml(&xml).unwrap();
        assert_eq!(mode, "GOVERNANCE");
        assert_eq!(parsed_date, date);
    }

    #[test]
    fn parse_retention_xml_missing_mode() {
        let xml = r#"<Retention>
  <RetainUntilDate>2027-01-01T00:00:00Z</RetainUntilDate>
</Retention>"#;

        let err = parse_retention_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn parse_retention_xml_bad_date() {
        let xml = r#"<Retention>
  <Mode>COMPLIANCE</Mode>
  <RetainUntilDate>not-a-date</RetainUntilDate>
</Retention>"#;

        let err = parse_retention_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidArgument);
    }

    #[test]
    fn parse_legal_hold_on() {
        let xml = r#"<LegalHold><Status>ON</Status></LegalHold>"#;
        assert_eq!(parse_legal_hold_xml(xml).unwrap(), "ON");
    }

    #[test]
    fn parse_legal_hold_off() {
        let xml = r#"<LegalHold><Status>OFF</Status></LegalHold>"#;
        assert_eq!(parse_legal_hold_xml(xml).unwrap(), "OFF");
    }

    #[test]
    fn parse_legal_hold_invalid() {
        let xml = r#"<LegalHold><Status>MAYBE</Status></LegalHold>"#;
        let err = parse_legal_hold_xml(xml).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::MalformedXML);
    }

    #[test]
    fn legal_hold_xml_roundtrip() {
        let xml = legal_hold_to_xml("ON");
        assert_eq!(parse_legal_hold_xml(&xml).unwrap(), "ON");

        let xml = legal_hold_to_xml("OFF");
        assert_eq!(parse_legal_hold_xml(&xml).unwrap(), "OFF");
    }

    #[test]
    fn compute_retain_until_days() {
        let now = Utc::now();
        let dr = DefaultRetention {
            mode: "GOVERNANCE".to_string(),
            days: Some(30),
            years: None,
        };
        let until = compute_retain_until(&dr, now);
        let diff = (until - now).num_days();
        assert_eq!(diff, 30);
    }

    #[test]
    fn compute_retain_until_years() {
        let now = Utc::now();
        let dr = DefaultRetention {
            mode: "COMPLIANCE".to_string(),
            days: None,
            years: Some(2),
        };
        let until = compute_retain_until(&dr, now);
        let diff = (until - now).num_days();
        assert_eq!(diff, 730);
    }

    use chrono::Datelike;
}
