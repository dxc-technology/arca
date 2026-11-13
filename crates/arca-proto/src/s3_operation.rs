//! Classification of an S3 request into the operation it performs.
//!
//! S3 overloads a handful of method + path pairs with query-string
//! subresources (`PUT /{bucket}?lifecycle`, `GET /{bucket}/{key}?tagging`).
//! The auth middleware decides which action to authorize from the request and
//! the handler decides which operation to run, so both must read the query
//! the same way: when they did not (a substring test in one place, a prefix
//! test in the other), `?xencryption` was authorized as GetBucketEncryption
//! and executed as a listing, and `?lifecycle&encryption` the other way round.
//!
//! Every decision lives here. Handlers dispatch with an exhaustive `match` on
//! the per-method enums, the middleware asks [`Operation::action`], so the two
//! cannot disagree. Rules:
//!
//! - query keys are compared exactly, after `application/x-www-form-urlencoded`
//!   decoding, so `?uploads`, `?uploads=` (mc) and `?x=1&uploads` are the same
//!   and `?xuploads` is not `uploads`;
//! - when several subresources are present, the first match in the order of
//!   each `classify` wins;
//! - a known subresource Arca does not implement is `Unimplemented` (501 from
//!   the handler, TD-007) rather than falling through to the plain operation:
//!   `DELETE /{bucket}?cors` must not delete the bucket.

use std::borrow::Cow;

use arca_core::policy::actions::*;
use http::Method;

/// TECHDEBT(TD-007): bucket subresources Arca does not implement.
const UNIMPLEMENTED_BUCKET_SUBRESOURCES: &[&str] = &[
    "acl",
    "cors",
    "logging",
    "policy",
    "policyStatus",
    "website",
    "ownershipControls",
    "publicAccessBlock",
    "accelerate",
    "requestPayment",
    "inventory",
    "analytics",
    "metrics",
    "intelligenttiering",
];

/// Bucket subresources Arca implements for at least one method. Present on a
/// method that does not handle it, it makes the request `Unimplemented`.
const IMPLEMENTED_BUCKET_SUBRESOURCES: &[&str] = &[
    "location",
    "encryption",
    "compression",
    "tagging",
    "lifecycle",
    "replication",
    "object-lock",
    "notification",
    "versioning",
    "uploads",
    "versions",
    "delete",
];

/// TECHDEBT(TD-007): object subresources Arca does not implement.
const UNIMPLEMENTED_OBJECT_SUBRESOURCES: &[&str] = &["acl", "torrent", "restore", "select"];

/// The decoded keys of a query string.
struct QueryKeys(Vec<String>);

impl QueryKeys {
    fn parse(query: &str) -> Self {
        Self(
            form_urlencoded::parse(query.as_bytes())
                .map(|(k, _)| k.into_owned())
                .collect(),
        )
    }

    fn has(&self, key: &str) -> bool {
        self.0.iter().any(|k| k == key)
    }

    fn has_any(&self, keys: &[&str]) -> bool {
        keys.iter().any(|k| self.has(k))
    }

    fn versioned(&self) -> bool {
        self.has("versionId")
    }
}

/// `GET /{bucket}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketGet {
    Location,
    Encryption,
    Compression,
    Tagging,
    Lifecycle,
    Replication,
    ObjectLock,
    Notification,
    Unimplemented,
    Versioning,
    ListMultipartUploads,
    ListObjectVersions,
    ListObjects,
}

impl BucketGet {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        if q.has("location") {
            Self::Location
        } else if q.has("encryption") {
            Self::Encryption
        } else if q.has("compression") {
            Self::Compression
        } else if q.has("tagging") {
            Self::Tagging
        } else if q.has("lifecycle") {
            Self::Lifecycle
        } else if q.has("replication") {
            Self::Replication
        } else if q.has("object-lock") {
            Self::ObjectLock
        } else if q.has("notification") {
            Self::Notification
        } else if q.has_any(UNIMPLEMENTED_BUCKET_SUBRESOURCES) || q.has("delete") {
            Self::Unimplemented
        } else if q.has("versioning") {
            Self::Versioning
        } else if q.has("uploads") {
            Self::ListMultipartUploads
        } else if q.has("versions") {
            Self::ListObjectVersions
        } else {
            Self::ListObjects
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Location => S3_GET_BUCKET_LOCATION,
            Self::Encryption => S3_GET_ENCRYPTION_CONFIGURATION,
            Self::Compression => ARCA_GET_BUCKET_COMPRESSION,
            Self::Tagging => S3_GET_BUCKET_TAGGING,
            Self::Lifecycle => S3_GET_LIFECYCLE_CONFIGURATION,
            Self::Replication => S3_GET_REPLICATION_CONFIGURATION,
            Self::ObjectLock => S3_GET_BUCKET_OBJECT_LOCK_CONFIGURATION,
            Self::Notification => S3_GET_BUCKET_NOTIFICATION,
            Self::Versioning => S3_GET_BUCKET_VERSIONING,
            Self::ListMultipartUploads => S3_LIST_BUCKET_MULTIPART_UPLOADS,
            Self::ListObjectVersions => S3_LIST_BUCKET_VERSIONS,
            Self::Unimplemented | Self::ListObjects => S3_LIST_BUCKET,
        }
    }
}

/// `PUT /{bucket}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketPut {
    Encryption,
    Compression,
    Versioning,
    Tagging,
    Replication,
    Lifecycle,
    ObjectLock,
    Notification,
    Unimplemented,
    CreateBucket,
}

impl BucketPut {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        if q.has("encryption") {
            Self::Encryption
        } else if q.has("compression") {
            Self::Compression
        } else if q.has("versioning") {
            Self::Versioning
        } else if q.has("tagging") {
            Self::Tagging
        } else if q.has("replication") {
            Self::Replication
        } else if q.has("lifecycle") {
            Self::Lifecycle
        } else if q.has("object-lock") {
            Self::ObjectLock
        } else if q.has("notification") {
            Self::Notification
        } else if q.has_any(UNIMPLEMENTED_BUCKET_SUBRESOURCES)
            || q.has_any(IMPLEMENTED_BUCKET_SUBRESOURCES)
        {
            Self::Unimplemented
        } else {
            Self::CreateBucket
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Encryption => S3_PUT_ENCRYPTION_CONFIGURATION,
            Self::Compression => ARCA_PUT_BUCKET_COMPRESSION,
            Self::Versioning => S3_PUT_BUCKET_VERSIONING,
            Self::Tagging => S3_PUT_BUCKET_TAGGING,
            Self::Replication => S3_PUT_REPLICATION_CONFIGURATION,
            Self::Lifecycle => S3_PUT_LIFECYCLE_CONFIGURATION,
            Self::ObjectLock => S3_PUT_BUCKET_OBJECT_LOCK_CONFIGURATION,
            Self::Notification => S3_PUT_BUCKET_NOTIFICATION,
            Self::Unimplemented | Self::CreateBucket => S3_CREATE_BUCKET,
        }
    }
}

/// `DELETE /{bucket}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketDelete {
    Tagging,
    Compression,
    Encryption,
    Replication,
    Lifecycle,
    Unimplemented,
    DeleteBucket,
}

impl BucketDelete {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        if q.has("tagging") {
            Self::Tagging
        } else if q.has("compression") {
            Self::Compression
        } else if q.has("encryption") {
            Self::Encryption
        } else if q.has("replication") {
            Self::Replication
        } else if q.has("lifecycle") {
            Self::Lifecycle
        } else if q.has_any(UNIMPLEMENTED_BUCKET_SUBRESOURCES)
            || q.has_any(IMPLEMENTED_BUCKET_SUBRESOURCES)
        {
            Self::Unimplemented
        } else {
            Self::DeleteBucket
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Tagging => S3_PUT_BUCKET_TAGGING,
            Self::Compression => ARCA_PUT_BUCKET_COMPRESSION,
            Self::Encryption => S3_PUT_ENCRYPTION_CONFIGURATION,
            Self::Replication => S3_PUT_REPLICATION_CONFIGURATION,
            Self::Lifecycle => S3_PUT_LIFECYCLE_CONFIGURATION,
            Self::Unimplemented | Self::DeleteBucket => S3_DELETE_BUCKET,
        }
    }
}

/// `POST /{bucket}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketPost {
    DeleteObjects,
    Unimplemented,
}

impl BucketPost {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        if q.has("delete") {
            Self::DeleteObjects
        } else {
            Self::Unimplemented
        }
    }

    /// `None` for DeleteObjects: AWS checks `s3:DeleteObject` per key, so the
    /// handler authorizes every key and the middleware checks nothing.
    fn action(self) -> Option<&'static str> {
        match self {
            Self::DeleteObjects => None,
            Self::Unimplemented => Some(S3_LIST_BUCKET),
        }
    }
}

/// `GET /{bucket}/{key}`. `versioned` is the presence of `versionId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectGet {
    Tagging { versioned: bool },
    Retention,
    LegalHold,
    ListParts,
    Attributes { versioned: bool },
    Unimplemented,
    GetObject { versioned: bool },
}

impl ObjectGet {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        let versioned = q.versioned();
        if q.has("tagging") {
            Self::Tagging { versioned }
        } else if q.has("retention") {
            Self::Retention
        } else if q.has("legal-hold") {
            Self::LegalHold
        } else if q.has("uploadId") {
            Self::ListParts
        } else if q.has("attributes") {
            Self::Attributes { versioned }
        } else if q.has_any(UNIMPLEMENTED_OBJECT_SUBRESOURCES) {
            Self::Unimplemented
        } else {
            Self::GetObject { versioned }
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Tagging { versioned: false } => S3_GET_OBJECT_TAGGING,
            Self::Tagging { versioned: true } => S3_GET_OBJECT_VERSION_TAGGING,
            Self::Retention => S3_GET_OBJECT_RETENTION,
            Self::LegalHold => S3_GET_OBJECT_LEGAL_HOLD,
            Self::ListParts => S3_LIST_MULTIPART_UPLOAD_PARTS,
            Self::Attributes { versioned: false } => S3_GET_OBJECT_ATTRIBUTES,
            Self::Attributes { versioned: true } => S3_GET_OBJECT_VERSION_ATTRIBUTES,
            Self::Unimplemented | Self::GetObject { versioned: false } => S3_GET_OBJECT,
            Self::GetObject { versioned: true } => S3_GET_OBJECT_VERSION,
        }
    }
}

/// `PUT /{bucket}/{key}`. Copies are PutObject / UploadPart too: the source
/// is authorized by the handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPut {
    Tagging { versioned: bool },
    Retention,
    LegalHold,
    Unimplemented,
    UploadPart,
    PutObject,
}

impl ObjectPut {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        if q.has("tagging") {
            Self::Tagging {
                versioned: q.versioned(),
            }
        } else if q.has("retention") {
            Self::Retention
        } else if q.has("legal-hold") {
            Self::LegalHold
        } else if q.has_any(UNIMPLEMENTED_OBJECT_SUBRESOURCES) {
            Self::Unimplemented
        } else if q.has("partNumber") && q.has("uploadId") {
            Self::UploadPart
        } else {
            Self::PutObject
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Tagging { versioned: false } => S3_PUT_OBJECT_TAGGING,
            Self::Tagging { versioned: true } => S3_PUT_OBJECT_VERSION_TAGGING,
            Self::Retention => S3_PUT_OBJECT_RETENTION,
            Self::LegalHold => S3_PUT_OBJECT_LEGAL_HOLD,
            Self::Unimplemented | Self::UploadPart | Self::PutObject => S3_PUT_OBJECT,
        }
    }
}

/// `DELETE /{bucket}/{key}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectDelete {
    Tagging { versioned: bool },
    AbortMultipartUpload,
    DeleteObject { versioned: bool },
}

impl ObjectDelete {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        let versioned = q.versioned();
        if q.has("tagging") {
            Self::Tagging { versioned }
        } else if q.has("uploadId") {
            Self::AbortMultipartUpload
        } else {
            Self::DeleteObject { versioned }
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Tagging { versioned: false } => S3_DELETE_OBJECT_TAGGING,
            Self::Tagging { versioned: true } => S3_DELETE_OBJECT_VERSION_TAGGING,
            Self::AbortMultipartUpload => S3_ABORT_MULTIPART_UPLOAD,
            Self::DeleteObject { versioned: false } => S3_DELETE_OBJECT,
            Self::DeleteObject { versioned: true } => S3_DELETE_OBJECT_VERSION,
        }
    }
}

/// `POST /{bucket}/{key}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectPost {
    CreateMultipartUpload,
    CompleteMultipartUpload,
    Unimplemented,
}

impl ObjectPost {
    pub fn classify(query: &str) -> Self {
        Self::from_keys(&QueryKeys::parse(query))
    }

    fn from_keys(q: &QueryKeys) -> Self {
        if q.has("uploads") {
            Self::CreateMultipartUpload
        } else if q.has("uploadId") {
            Self::CompleteMultipartUpload
        } else {
            Self::Unimplemented
        }
    }
}

/// The operation a request performs, as the router and the handlers see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    ListBuckets,
    BucketGet(BucketGet),
    HeadBucket,
    BucketPut(BucketPut),
    BucketDelete(BucketDelete),
    BucketPost(BucketPost),
    ObjectGet(ObjectGet),
    HeadObject { versioned: bool },
    ObjectPut(ObjectPut),
    ObjectDelete(ObjectDelete),
    ObjectPost(ObjectPost),
    /// A method the router does not route (it answers 405).
    OtherBucketMethod,
    OtherObjectMethod,
}

impl Operation {
    /// Classifies a request from its method, its routed bucket and key
    /// (already split and decoded) and its raw query string.
    pub fn classify(method: &Method, bucket: &str, key: Option<&str>, query: &str) -> Self {
        let q = QueryKeys::parse(query);
        if bucket.is_empty() {
            return Self::ListBuckets;
        }
        match key.filter(|k| !k.is_empty()) {
            None => match *method {
                Method::GET => Self::BucketGet(BucketGet::from_keys(&q)),
                Method::HEAD => Self::HeadBucket,
                Method::PUT => Self::BucketPut(BucketPut::from_keys(&q)),
                Method::DELETE => Self::BucketDelete(BucketDelete::from_keys(&q)),
                Method::POST => Self::BucketPost(BucketPost::from_keys(&q)),
                _ => Self::OtherBucketMethod,
            },
            Some(_) => match *method {
                Method::GET => Self::ObjectGet(ObjectGet::from_keys(&q)),
                Method::HEAD => Self::HeadObject {
                    versioned: q.versioned(),
                },
                Method::PUT => Self::ObjectPut(ObjectPut::from_keys(&q)),
                Method::DELETE => Self::ObjectDelete(ObjectDelete::from_keys(&q)),
                Method::POST => Self::ObjectPost(ObjectPost::from_keys(&q)),
                _ => Self::OtherObjectMethod,
            },
        }
    }

    /// The IAM action the auth middleware evaluates, or `None` when the
    /// handler authorizes every object it touches instead (DeleteObjects).
    pub fn action(self) -> Option<&'static str> {
        Some(match self {
            Self::ListBuckets => S3_LIST_ALL_MY_BUCKETS,
            Self::BucketGet(op) => op.action(),
            Self::HeadBucket => S3_LIST_BUCKET,
            Self::BucketPut(op) => op.action(),
            Self::BucketDelete(op) => op.action(),
            Self::BucketPost(op) => return op.action(),
            Self::ObjectGet(op) => op.action(),
            Self::HeadObject { versioned: false } => S3_GET_OBJECT,
            Self::HeadObject { versioned: true } => S3_GET_OBJECT_VERSION,
            Self::ObjectPut(op) => op.action(),
            Self::ObjectDelete(op) => op.action(),
            Self::ObjectPost(_) => S3_PUT_OBJECT,
            Self::OtherBucketMethod => S3_LIST_BUCKET,
            Self::OtherObjectMethod => S3_GET_OBJECT,
        })
    }
}

impl Operation {
    /// The S3 API operation name the audit log and the request metrics record.
    pub fn name(self) -> &'static str {
        match self {
            Self::ListBuckets => "ListBuckets",
            Self::BucketGet(op) => match op {
                BucketGet::Location => "GetBucketLocation",
                BucketGet::Encryption => "GetBucketEncryption",
                BucketGet::Compression => "GetBucketCompression",
                BucketGet::Tagging => "GetBucketTagging",
                BucketGet::Lifecycle => "GetBucketLifecycleConfiguration",
                BucketGet::Replication => "GetBucketReplication",
                BucketGet::ObjectLock => "GetObjectLockConfiguration",
                BucketGet::Notification => "GetBucketNotificationConfiguration",
                BucketGet::Unimplemented => "NotImplemented",
                BucketGet::Versioning => "GetBucketVersioning",
                BucketGet::ListMultipartUploads => "ListMultipartUploads",
                BucketGet::ListObjectVersions => "ListObjectVersions",
                BucketGet::ListObjects => "ListObjectsV1",
            },
            Self::HeadBucket => "HeadBucket",
            Self::BucketPut(op) => match op {
                BucketPut::Encryption => "PutBucketEncryption",
                BucketPut::Compression => "PutBucketCompression",
                BucketPut::Versioning => "PutBucketVersioning",
                BucketPut::Tagging => "PutBucketTagging",
                BucketPut::Replication => "PutBucketReplication",
                BucketPut::Lifecycle => "PutBucketLifecycleConfiguration",
                BucketPut::ObjectLock => "PutObjectLockConfiguration",
                BucketPut::Notification => "PutBucketNotificationConfiguration",
                BucketPut::Unimplemented => "NotImplemented",
                BucketPut::CreateBucket => "CreateBucket",
            },
            Self::BucketDelete(op) => match op {
                BucketDelete::Tagging => "DeleteBucketTagging",
                BucketDelete::Compression => "DeleteBucketCompression",
                BucketDelete::Encryption => "DeleteBucketEncryption",
                BucketDelete::Replication => "DeleteBucketReplication",
                BucketDelete::Lifecycle => "DeleteBucketLifecycleConfiguration",
                BucketDelete::Unimplemented => "NotImplemented",
                BucketDelete::DeleteBucket => "DeleteBucket",
            },
            Self::BucketPost(op) => match op {
                BucketPost::DeleteObjects => "DeleteObjects",
                BucketPost::Unimplemented => "PostBucket",
            },
            Self::ObjectGet(op) => match op {
                ObjectGet::Tagging { .. } => "GetObjectTagging",
                ObjectGet::Retention => "GetObjectRetention",
                ObjectGet::LegalHold => "GetObjectLegalHold",
                ObjectGet::ListParts => "ListParts",
                ObjectGet::Attributes { .. } => "GetObjectAttributes",
                ObjectGet::Unimplemented => "NotImplemented",
                ObjectGet::GetObject { .. } => "GetObject",
            },
            Self::HeadObject { .. } => "HeadObject",
            Self::ObjectPut(op) => match op {
                ObjectPut::Tagging { .. } => "PutObjectTagging",
                ObjectPut::Retention => "PutObjectRetention",
                ObjectPut::LegalHold => "PutObjectLegalHold",
                ObjectPut::Unimplemented => "NotImplemented",
                ObjectPut::UploadPart => "UploadPart",
                ObjectPut::PutObject => "PutObject",
            },
            Self::ObjectDelete(op) => match op {
                ObjectDelete::Tagging { .. } => "DeleteObjectTagging",
                ObjectDelete::AbortMultipartUpload => "AbortMultipartUpload",
                ObjectDelete::DeleteObject { .. } => "DeleteObject",
            },
            Self::ObjectPost(op) => match op {
                ObjectPost::CreateMultipartUpload => "CreateMultipartUpload",
                ObjectPost::CompleteMultipartUpload => "CompleteMultipartUpload",
                ObjectPost::Unimplemented => "PostObject",
            },
            Self::OtherBucketMethod | Self::OtherObjectMethod => "Unknown",
        }
    }
}

/// Splits a routed path into its bucket and key and percent-decodes both,
/// exactly as the router's `Path` extractor does: split on the first `/` of
/// the raw path (an encoded `%2F` stays inside its segment), then decode.
/// `None` when a segment does not decode to UTF-8 (the router rejects it).
pub fn split_routed_path(path: &str) -> Option<(Cow<'_, str>, Option<Cow<'_, str>>)> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let (raw_bucket, raw_key) = match trimmed.split_once('/') {
        Some((b, k)) => (b, Some(k)),
        None => (trimmed, None),
    };
    let bucket = urlencoding::decode(raw_bucket).ok()?;
    let key = match raw_key {
        Some(k) => Some(urlencoding::decode(k).ok()?),
        None => None,
    };
    Some((bucket, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(method: Method, path: &str, query: &str) -> Option<&'static str> {
        let (bucket, key) = split_routed_path(path).expect("decodable path");
        Operation::classify(&method, &bucket, key.as_deref(), query).action()
    }

    /// One row per operation: method, path, query, expected action.
    #[test]
    fn every_operation_has_its_aws_action() {
        use Method as M;
        let table: &[(M, &str, &str, Option<&str>)] = &[
            (M::GET, "/", "", Some("s3:ListAllMyBuckets")),
            // Bucket GET
            (M::GET, "/b", "", Some("s3:ListBucket")),
            (M::GET, "/b", "list-type=2&prefix=a", Some("s3:ListBucket")),
            (M::GET, "/b", "location", Some("s3:GetBucketLocation")),
            (M::GET, "/b", "encryption", Some("s3:GetEncryptionConfiguration")),
            (M::GET, "/b", "compression", Some("arca:GetBucketCompression")),
            (M::GET, "/b", "tagging", Some("s3:GetBucketTagging")),
            (M::GET, "/b", "lifecycle", Some("s3:GetLifecycleConfiguration")),
            (M::GET, "/b", "replication", Some("s3:GetReplicationConfiguration")),
            (M::GET, "/b", "object-lock", Some("s3:GetBucketObjectLockConfiguration")),
            (M::GET, "/b", "notification", Some("s3:GetBucketNotification")),
            (M::GET, "/b", "versioning", Some("s3:GetBucketVersioning")),
            (M::GET, "/b", "uploads", Some("s3:ListBucketMultipartUploads")),
            (M::GET, "/b", "uploads=&prefix=a", Some("s3:ListBucketMultipartUploads")),
            (M::GET, "/b", "versions&prefix=a", Some("s3:ListBucketVersions")),
            (M::GET, "/b", "cors", Some("s3:ListBucket")),
            (M::HEAD, "/b", "", Some("s3:ListBucket")),
            // Bucket PUT
            (M::PUT, "/b", "", Some("s3:CreateBucket")),
            (M::PUT, "/b", "encryption", Some("s3:PutEncryptionConfiguration")),
            (M::PUT, "/b", "compression", Some("arca:PutBucketCompression")),
            (M::PUT, "/b", "versioning", Some("s3:PutBucketVersioning")),
            (M::PUT, "/b", "tagging", Some("s3:PutBucketTagging")),
            (M::PUT, "/b", "replication", Some("s3:PutReplicationConfiguration")),
            (M::PUT, "/b", "lifecycle", Some("s3:PutLifecycleConfiguration")),
            (M::PUT, "/b", "object-lock", Some("s3:PutBucketObjectLockConfiguration")),
            (M::PUT, "/b", "notification", Some("s3:PutBucketNotification")),
            // Bucket DELETE: removing a configuration needs the Put action.
            (M::DELETE, "/b", "", Some("s3:DeleteBucket")),
            (M::DELETE, "/b", "tagging", Some("s3:PutBucketTagging")),
            (M::DELETE, "/b", "compression", Some("arca:PutBucketCompression")),
            (M::DELETE, "/b", "encryption", Some("s3:PutEncryptionConfiguration")),
            (M::DELETE, "/b", "replication", Some("s3:PutReplicationConfiguration")),
            (M::DELETE, "/b", "lifecycle", Some("s3:PutLifecycleConfiguration")),
            // Bucket POST
            (M::POST, "/b", "delete", None),
            (M::POST, "/b", "delete=", None),
            (M::POST, "/b", "x=1&delete", None),
            (M::POST, "/b", "undelete", Some("s3:ListBucket")),
            // Object GET / HEAD
            (M::GET, "/b/k", "", Some("s3:GetObject")),
            (M::GET, "/b/k", "versionId=v", Some("s3:GetObjectVersion")),
            (M::GET, "/b/k", "partNumber=1", Some("s3:GetObject")),
            (M::GET, "/b/k", "tagging", Some("s3:GetObjectTagging")),
            (M::GET, "/b/k", "tagging&versionId=v", Some("s3:GetObjectVersionTagging")),
            (M::GET, "/b/k", "retention", Some("s3:GetObjectRetention")),
            (M::GET, "/b/k", "legal-hold", Some("s3:GetObjectLegalHold")),
            (M::GET, "/b/k", "uploadId=u", Some("s3:ListMultipartUploadParts")),
            (M::GET, "/b/k", "attributes", Some("s3:GetObjectAttributes")),
            (M::GET, "/b/k", "attributes&versionId=v", Some("s3:GetObjectVersionAttributes")),
            (M::HEAD, "/b/k", "", Some("s3:GetObject")),
            (M::HEAD, "/b/k", "versionId=v", Some("s3:GetObjectVersion")),
            // Object PUT
            (M::PUT, "/b/k", "", Some("s3:PutObject")),
            (M::PUT, "/b/k", "partNumber=1&uploadId=u", Some("s3:PutObject")),
            (M::PUT, "/b/k", "tagging", Some("s3:PutObjectTagging")),
            (M::PUT, "/b/k", "tagging&versionId=v", Some("s3:PutObjectVersionTagging")),
            (M::PUT, "/b/k", "retention", Some("s3:PutObjectRetention")),
            (M::PUT, "/b/k", "legal-hold", Some("s3:PutObjectLegalHold")),
            // Object DELETE
            (M::DELETE, "/b/k", "", Some("s3:DeleteObject")),
            (M::DELETE, "/b/k", "versionId=v", Some("s3:DeleteObjectVersion")),
            (M::DELETE, "/b/k", "uploadId=u", Some("s3:AbortMultipartUpload")),
            (M::DELETE, "/b/k", "tagging", Some("s3:DeleteObjectTagging")),
            (M::DELETE, "/b/k", "tagging&versionId=v", Some("s3:DeleteObjectVersionTagging")),
            // Object POST
            (M::POST, "/b/k", "uploads", Some("s3:PutObject")),
            (M::POST, "/b/k", "uploads=", Some("s3:PutObject")),
            (M::POST, "/b/k", "uploadId=u", Some("s3:PutObject")),
        ];
        for (method, path, query, expected) in table {
            assert_eq!(
                action(method.clone(), path, query),
                *expected,
                "{method} {path}?{query}"
            );
        }
    }

    /// V4-V6 of the report: a key that merely contains a subresource name is
    /// not that subresource, for the middleware and the handler alike.
    #[test]
    fn subresource_keys_match_exactly() {
        assert_eq!(BucketGet::classify("xencryption"), BucketGet::ListObjects);
        assert_eq!(BucketGet::classify("prefix=encryption"), BucketGet::ListObjects);
        assert_eq!(BucketPut::classify("xencryption"), BucketPut::CreateBucket);
        assert_eq!(BucketDelete::classify("xencryption"), BucketDelete::DeleteBucket);
        assert_eq!(BucketPost::classify("x&deletex"), BucketPost::Unimplemented);
        assert_eq!(
            ObjectGet::classify("response-content-type=tagging"),
            ObjectGet::GetObject { versioned: false }
        );
        assert_eq!(ObjectPost::classify("xuploads"), ObjectPost::Unimplemented);
    }

    /// The inverse mismatch: a subresource that is not the first query key is
    /// still that subresource, and with two of them both sides pick the same.
    #[test]
    fn a_subresource_anywhere_in_the_query_counts() {
        assert_eq!(BucketPut::classify("x=1&versioning"), BucketPut::Versioning);
        assert_eq!(BucketPut::classify("lifecycle&encryption"), BucketPut::Encryption);
        assert_eq!(BucketDelete::classify("lifecycle&encryption"), BucketDelete::Encryption);
        assert_eq!(
            action(Method::PUT, "/b", "lifecycle&encryption"),
            Some("s3:PutEncryptionConfiguration")
        );
    }

    /// An encoded query key is decoded before matching, on both sides.
    #[test]
    fn query_keys_are_decoded() {
        assert_eq!(BucketPut::classify("%6Cifecycle"), BucketPut::Lifecycle);
    }

    /// A known subresource Arca does not implement never falls through to the
    /// plain operation: `DELETE ?cors` must not delete the bucket, `PUT ?acl`
    /// on a key must not overwrite the object with the ACL document.
    #[test]
    fn unimplemented_subresources_are_not_the_plain_operation() {
        for q in ["cors", "policy", "website", "versioning", "uploads", "location"] {
            assert_eq!(BucketDelete::classify(q), BucketDelete::Unimplemented, "{q}");
        }
        for q in ["cors", "acl", "location", "uploads", "versions"] {
            assert_eq!(BucketPut::classify(q), BucketPut::Unimplemented, "{q}");
        }
        assert_eq!(BucketGet::classify("cors"), BucketGet::Unimplemented);
        assert_eq!(ObjectPut::classify("acl"), ObjectPut::Unimplemented);
        assert_eq!(ObjectGet::classify("acl"), ObjectGet::Unimplemented);
        assert_eq!(ObjectPost::classify("restore"), ObjectPost::Unimplemented);
    }

    #[test]
    fn upload_part_needs_both_parameters() {
        assert_eq!(ObjectPut::classify("partNumber=1&uploadId=u"), ObjectPut::UploadPart);
        assert_eq!(ObjectPut::classify("partNumber=1"), ObjectPut::PutObject);
    }

    #[test]
    fn routed_paths_are_decoded_like_the_router() {
        let (b, k) = split_routed_path("/fin%61nce/privat%65/a%2Fb").unwrap();
        assert_eq!((b.as_ref(), k.as_deref()), ("finance", Some("private/a/b")));
        let (b, k) = split_routed_path("/b").unwrap();
        assert_eq!((b.as_ref(), k), ("b", None));
        assert!(split_routed_path("/b/%FF").is_none());
        assert!(split_routed_path("/%FF").is_none());
    }
}
