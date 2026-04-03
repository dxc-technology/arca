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

    #[error("decryption failed: {0}")]
    DecryptionFailed(String),

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
    EntityTooLarge,
    EntityTooSmall,
    InternalError,
    InvalidAccessKeyId,
    InvalidArgument,
    InvalidBucketName,
    InvalidPart,
    InvalidPartOrder,
    InvalidRange,
    InvalidRequest,
    MethodNotAllowed,
    NotModified,
    NoSuchBucket,
    NoSuchKey,
    NoSuchVersion,
    MalformedXML,
    PreconditionFailed,
    NoSuchLifecycleConfiguration,
    NoSuchObjectLockConfiguration,
    NoSuchTagSet,
    ObjectLocked,
    NoSuchUpload,
    NotImplemented,
    InvalidTag,
    ServerSideEncryptionConfigurationNotFoundError,
    SignatureDoesNotMatch,
    SlowDown,
    InvalidRetentionPeriod,
    InvalidBucketState,
}

impl S3ErrorCode {
    /// Returns the HTTP status code for this error.
    pub fn http_status(&self) -> u16 {
        match self {
            S3ErrorCode::AccessDenied => 403,
            S3ErrorCode::BadDigest => 400,
            S3ErrorCode::BucketAlreadyExists => 409,
            S3ErrorCode::BucketAlreadyOwnedByYou => 200,
            S3ErrorCode::BucketNotEmpty => 409,
            S3ErrorCode::EntityTooLarge => 400,
            S3ErrorCode::EntityTooSmall => 400,
            S3ErrorCode::InternalError => 500,
            S3ErrorCode::InvalidAccessKeyId => 403,
            S3ErrorCode::InvalidArgument => 400,
            S3ErrorCode::InvalidBucketName => 400,
            S3ErrorCode::InvalidPart => 400,
            S3ErrorCode::InvalidPartOrder => 400,
            S3ErrorCode::InvalidRange => 416,
            S3ErrorCode::InvalidRequest => 400,
            S3ErrorCode::MethodNotAllowed => 405,
            S3ErrorCode::MalformedXML => 400,
            S3ErrorCode::NotModified => 304,
            S3ErrorCode::NoSuchBucket => 404,
            S3ErrorCode::NoSuchKey => 404,
            S3ErrorCode::NoSuchVersion => 404,
            S3ErrorCode::NoSuchLifecycleConfiguration => 404,
            S3ErrorCode::NoSuchObjectLockConfiguration => 404,
            S3ErrorCode::NoSuchTagSet => 404,
            S3ErrorCode::ObjectLocked => 403,
            S3ErrorCode::NoSuchUpload => 404,
            S3ErrorCode::NotImplemented => 501,
            S3ErrorCode::InvalidTag => 400,
            S3ErrorCode::PreconditionFailed => 412,
            S3ErrorCode::ServerSideEncryptionConfigurationNotFoundError => 400,
            S3ErrorCode::SignatureDoesNotMatch => 403,
            S3ErrorCode::SlowDown => 503,
            S3ErrorCode::InvalidRetentionPeriod => 400,
            S3ErrorCode::InvalidBucketState => 409,
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
            S3ErrorCode::EntityTooLarge => "EntityTooLarge",
            S3ErrorCode::EntityTooSmall => "EntityTooSmall",
            S3ErrorCode::InternalError => "InternalError",
            S3ErrorCode::InvalidAccessKeyId => "InvalidAccessKeyId",
            S3ErrorCode::InvalidArgument => "InvalidArgument",
            S3ErrorCode::InvalidBucketName => "InvalidBucketName",
            S3ErrorCode::InvalidPart => "InvalidPart",
            S3ErrorCode::InvalidPartOrder => "InvalidPartOrder",
            S3ErrorCode::InvalidRange => "InvalidRange",
            S3ErrorCode::InvalidRequest => "InvalidRequest",
            S3ErrorCode::MethodNotAllowed => "MethodNotAllowed",
            S3ErrorCode::MalformedXML => "MalformedXML",
            S3ErrorCode::NotModified => "NotModified",
            S3ErrorCode::NoSuchBucket => "NoSuchBucket",
            S3ErrorCode::NoSuchKey => "NoSuchKey",
            S3ErrorCode::NoSuchVersion => "NoSuchVersion",
            S3ErrorCode::NoSuchLifecycleConfiguration => "NoSuchLifecycleConfiguration",
            S3ErrorCode::NoSuchObjectLockConfiguration => {
                "ObjectLockConfigurationNotFoundError"
            }
            S3ErrorCode::NoSuchTagSet => "NoSuchTagSet",
            S3ErrorCode::ObjectLocked => "AccessDenied",
            S3ErrorCode::NoSuchUpload => "NoSuchUpload",
            S3ErrorCode::NotImplemented => "NotImplemented",
            S3ErrorCode::InvalidTag => "InvalidTag",
            S3ErrorCode::PreconditionFailed => "PreconditionFailed",
            S3ErrorCode::ServerSideEncryptionConfigurationNotFoundError => {
                "ServerSideEncryptionConfigurationNotFoundError"
            }
            S3ErrorCode::SignatureDoesNotMatch => "SignatureDoesNotMatch",
            S3ErrorCode::SlowDown => "SlowDown",
            S3ErrorCode::InvalidRetentionPeriod => "InvalidRetentionPeriod",
            S3ErrorCode::InvalidBucketState => "InvalidBucketState",
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
            S3ErrorCode::EntityTooLarge => {
                "Your proposed upload exceeds the maximum allowed object size."
            }
            S3ErrorCode::EntityTooSmall => {
                "Your proposed upload is smaller than the minimum allowed object size."
            }
            S3ErrorCode::InternalError => {
                "We encountered an internal error. Please try again."
            }
            S3ErrorCode::InvalidAccessKeyId => {
                "The AWS access key ID you provided does not exist in our records."
            }
            S3ErrorCode::InvalidArgument => "Invalid Argument",
            S3ErrorCode::InvalidBucketName => "The specified bucket is not valid.",
            S3ErrorCode::InvalidPart => {
                "One or more of the specified parts could not be found."
            }
            S3ErrorCode::InvalidPartOrder => {
                "The list of parts was not in ascending order."
            }
            S3ErrorCode::InvalidRange => {
                "The requested range is not satisfiable."
            }
            S3ErrorCode::InvalidRequest => "Invalid Request",
            S3ErrorCode::MethodNotAllowed => {
                "The specified method is not allowed against this resource."
            }
            S3ErrorCode::MalformedXML => {
                "The XML you provided was not well-formed or did not validate against our published schema."
            }
            S3ErrorCode::NotModified => "Not Modified",
            S3ErrorCode::NoSuchBucket => "The specified bucket does not exist.",
            S3ErrorCode::NoSuchKey => "The specified key does not exist.",
            S3ErrorCode::NoSuchVersion => "The specified version does not exist.",
            S3ErrorCode::NoSuchLifecycleConfiguration => {
                "The lifecycle configuration does not exist."
            }
            S3ErrorCode::NoSuchObjectLockConfiguration => {
                "Object Lock configuration does not exist for this bucket."
            }
            S3ErrorCode::NoSuchTagSet => "The TagSet does not exist.",
            S3ErrorCode::ObjectLocked => {
                "Object is protected by Object Lock and cannot be deleted."
            }
            S3ErrorCode::NoSuchUpload => {
                "The specified multipart upload does not exist."
            }
            S3ErrorCode::NotImplemented => {
                "A header you provided implies functionality that is not implemented."
            }
            S3ErrorCode::InvalidTag => "The tag provided was not valid.",
            S3ErrorCode::PreconditionFailed => {
                "At least one of the pre-conditions you specified did not hold."
            }
            S3ErrorCode::ServerSideEncryptionConfigurationNotFoundError => {
                "The server side encryption configuration was not found."
            }
            S3ErrorCode::SignatureDoesNotMatch => {
                "The request signature we calculated does not match the signature you provided."
            }
            S3ErrorCode::SlowDown => {
                "Please reduce your request rate."
            }
            S3ErrorCode::InvalidRetentionPeriod => {
                "The retention period specified is not valid."
            }
            S3ErrorCode::InvalidBucketState => {
                "The request is not valid for the current state of the bucket."
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
        assert_eq!(S3ErrorCode::NoSuchUpload.http_status(), 404);
        assert_eq!(S3ErrorCode::BucketAlreadyExists.http_status(), 409);
        assert_eq!(S3ErrorCode::BucketAlreadyOwnedByYou.http_status(), 200);
        assert_eq!(S3ErrorCode::BucketNotEmpty.http_status(), 409);
        assert_eq!(S3ErrorCode::InvalidBucketName.http_status(), 400);
        assert_eq!(S3ErrorCode::InvalidPart.http_status(), 400);
        assert_eq!(S3ErrorCode::InvalidPartOrder.http_status(), 400);
        assert_eq!(S3ErrorCode::EntityTooSmall.http_status(), 400);
        assert_eq!(S3ErrorCode::AccessDenied.http_status(), 403);
        assert_eq!(S3ErrorCode::InvalidAccessKeyId.http_status(), 403);
        assert_eq!(S3ErrorCode::SignatureDoesNotMatch.http_status(), 403);
        assert_eq!(S3ErrorCode::BadDigest.http_status(), 400);
        assert_eq!(S3ErrorCode::InvalidArgument.http_status(), 400);
        assert_eq!(S3ErrorCode::InternalError.http_status(), 500);
        assert_eq!(S3ErrorCode::MethodNotAllowed.http_status(), 405);
        assert_eq!(S3ErrorCode::MalformedXML.http_status(), 400);
        assert_eq!(S3ErrorCode::NotModified.http_status(), 304);
        assert_eq!(S3ErrorCode::NoSuchVersion.http_status(), 404);
        assert_eq!(S3ErrorCode::PreconditionFailed.http_status(), 412);
        assert_eq!(
            S3ErrorCode::ServerSideEncryptionConfigurationNotFoundError.http_status(),
            400
        );
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
