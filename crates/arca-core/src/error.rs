//! Error types for Arca.
//!
//! - [`ArcaError`]: Internal errors (thiserror-based).
//! - [`S3ErrorCode`]: S3 API error codes with HTTP status mapping.
//! - [`S3Error`]: Full S3 error with XML serialization.

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

/// Internal Arca error type.
#[derive(Debug, thiserror::Error)]
pub enum ArcaError {
    #[error("S3 error: {0}")]
    S3(#[from] S3Error),

    #[error("internal error: {0}")]
    Internal(String),
}

/// S3 API error codes with HTTP status mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3ErrorCode {
    AccessDenied,
    BadDigest,
    BucketAlreadyExists,
    BucketAlreadyOwnedByYou,
    BucketNotEmpty,
    InternalError,
    InvalidBucketName,
    NoSuchBucket,
    NoSuchKey,
    NotImplemented,
    SignatureDoesNotMatch,
}

impl S3ErrorCode {
    /// Returns the HTTP status code for this error.
    pub fn http_status(&self) -> u16 {
        match self {
            S3ErrorCode::AccessDenied => 403,
            S3ErrorCode::BadDigest => 400,
            S3ErrorCode::BucketAlreadyExists => 409,
            S3ErrorCode::BucketAlreadyOwnedByYou => 409,
            S3ErrorCode::BucketNotEmpty => 409,
            S3ErrorCode::InternalError => 500,
            S3ErrorCode::InvalidBucketName => 400,
            S3ErrorCode::NoSuchBucket => 404,
            S3ErrorCode::NoSuchKey => 404,
            S3ErrorCode::NotImplemented => 501,
            S3ErrorCode::SignatureDoesNotMatch => 403,
        }
    }

    /// Returns the S3 error code string (e.g. "NotImplemented").
    pub fn as_str(&self) -> &'static str {
        match self {
            S3ErrorCode::AccessDenied => "AccessDenied",
            S3ErrorCode::BadDigest => "BadDigest",
            S3ErrorCode::BucketAlreadyExists => "BucketAlreadyExists",
            S3ErrorCode::BucketAlreadyOwnedByYou => "BucketAlreadyOwnedByYou",
            S3ErrorCode::BucketNotEmpty => "BucketNotEmpty",
            S3ErrorCode::InternalError => "InternalError",
            S3ErrorCode::InvalidBucketName => "InvalidBucketName",
            S3ErrorCode::NoSuchBucket => "NoSuchBucket",
            S3ErrorCode::NoSuchKey => "NoSuchKey",
            S3ErrorCode::NotImplemented => "NotImplemented",
            S3ErrorCode::SignatureDoesNotMatch => "SignatureDoesNotMatch",
        }
    }

    /// Returns the default human-readable message for this error code.
    pub fn default_message(&self) -> &'static str {
        match self {
            S3ErrorCode::AccessDenied => "Access Denied",
            S3ErrorCode::BadDigest => {
                "The Content-MD5 you specified did not match what we received."
            }
            S3ErrorCode::BucketAlreadyExists => {
                "The requested bucket name is not available."
            }
            S3ErrorCode::BucketAlreadyOwnedByYou => {
                "The bucket you tried to create already exists, and you own it."
            }
            S3ErrorCode::BucketNotEmpty => {
                "The bucket you tried to delete is not empty."
            }
            S3ErrorCode::InternalError => {
                "We encountered an internal error. Please try again."
            }
            S3ErrorCode::InvalidBucketName => "The specified bucket is not valid.",
            S3ErrorCode::NoSuchBucket => "The specified bucket does not exist.",
            S3ErrorCode::NoSuchKey => "The specified key does not exist.",
            S3ErrorCode::NotImplemented => {
                "A header you provided implies functionality that is not implemented."
            }
            S3ErrorCode::SignatureDoesNotMatch => {
                "The request signature we calculated does not match the signature you provided."
            }
        }
    }
}

impl std::fmt::Display for S3ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A complete S3 error response, serializable to XML.
#[derive(Debug, Clone, thiserror::Error)]
#[error("S3 error {code}: {message}")]
pub struct S3Error {
    pub code: S3ErrorCode,
    pub message: String,
    pub resource: String,
    pub request_id: String,
}

impl S3Error {
    /// Creates a new S3 error with the default message for the given code.
    pub fn new(code: S3ErrorCode, resource: impl Into<String>) -> Self {
        Self {
            message: code.default_message().to_string(),
            code,
            resource: resource.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Creates a new S3 error with a custom message.
    pub fn with_message(
        code: S3ErrorCode,
        message: impl Into<String>,
        resource: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            resource: resource.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// Returns the HTTP status code for this error.
    pub fn http_status(&self) -> u16 {
        self.code.http_status()
    }

    /// Serializes this error to S3-compatible XML.
    pub fn to_xml(&self) -> String {
        let mut writer = Writer::new(Vec::new());

        writer
            .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
            .expect("write XML decl");

        // <Error>
        writer
            .write_event(Event::Start(BytesStart::new("Error")))
            .expect("write Error start");

        write_xml_element(&mut writer, "Code", self.code.as_str());
        write_xml_element(&mut writer, "Message", &self.message);
        write_xml_element(&mut writer, "Resource", &self.resource);
        write_xml_element(&mut writer, "RequestId", &self.request_id);

        // </Error>
        writer
            .write_event(Event::End(BytesEnd::new("Error")))
            .expect("write Error end");

        String::from_utf8(writer.into_inner()).expect("valid UTF-8 XML")
    }
}

/// Helper to write a simple `<Tag>text</Tag>` XML element.
pub(crate) fn write_xml_element(writer: &mut Writer<Vec<u8>>, tag: &str, text: &str) {
    writer
        .write_event(Event::Start(BytesStart::new(tag)))
        .expect("write element start");
    writer
        .write_event(Event::Text(BytesText::new(text)))
        .expect("write element text");
    writer
        .write_event(Event::End(BytesEnd::new(tag)))
        .expect("write element end");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_error_xml_format() {
        let err = S3Error {
            code: S3ErrorCode::NotImplemented,
            message: "Not implemented".to_string(),
            resource: "/".to_string(),
            request_id: "test-request-id".to_string(),
        };

        let xml = err.to_xml();

        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
        assert!(xml.contains("<Code>NotImplemented</Code>"));
        assert!(xml.contains("<Message>Not implemented</Message>"));
        assert!(xml.contains("<Resource>/</Resource>"));
        assert!(xml.contains("<RequestId>test-request-id</RequestId>"));
        assert!(xml.contains("<Error>"));
        assert!(xml.contains("</Error>"));
    }

    #[test]
    fn s3_error_xml_no_namespace() {
        let err = S3Error::new(S3ErrorCode::NoSuchBucket, "/my-bucket");
        let xml = err.to_xml();

        // Should not contain any xmlns attribute
        assert!(!xml.contains("xmlns"));
    }

    #[test]
    fn s3_error_code_http_status_mapping() {
        assert_eq!(S3ErrorCode::NotImplemented.http_status(), 501);
        assert_eq!(S3ErrorCode::NoSuchBucket.http_status(), 404);
        assert_eq!(S3ErrorCode::NoSuchKey.http_status(), 404);
        assert_eq!(S3ErrorCode::BucketAlreadyExists.http_status(), 409);
        assert_eq!(S3ErrorCode::BucketAlreadyOwnedByYou.http_status(), 409);
        assert_eq!(S3ErrorCode::BucketNotEmpty.http_status(), 409);
        assert_eq!(S3ErrorCode::InvalidBucketName.http_status(), 400);
        assert_eq!(S3ErrorCode::AccessDenied.http_status(), 403);
        assert_eq!(S3ErrorCode::SignatureDoesNotMatch.http_status(), 403);
        assert_eq!(S3ErrorCode::BadDigest.http_status(), 400);
        assert_eq!(S3ErrorCode::InternalError.http_status(), 500);
    }

    #[test]
    fn s3_error_new_uses_default_message() {
        let err = S3Error::new(S3ErrorCode::NoSuchKey, "/bucket/key");
        assert_eq!(err.message, "The specified key does not exist.");
        assert_eq!(err.resource, "/bucket/key");
        assert!(!err.request_id.is_empty());
    }

    #[test]
    fn s3_error_with_message_uses_custom() {
        let err =
            S3Error::with_message(S3ErrorCode::InternalError, "custom error", "/resource");
        assert_eq!(err.message, "custom error");
    }

    #[test]
    fn s3_error_code_display() {
        assert_eq!(format!("{}", S3ErrorCode::NotImplemented), "NotImplemented");
        assert_eq!(format!("{}", S3ErrorCode::NoSuchBucket), "NoSuchBucket");
    }
}
