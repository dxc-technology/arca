//! Authorization helper for S3 and admin request handlers.
//!
//! Extracts the `AuthenticatedIdentity` from request extensions and evaluates
//! the user's effective policies against the requested action and resource.

use arca_core::policy::{self, Evaluation};
use arca_core::{S3Error, S3ErrorCode};

use crate::middleware::identity::AuthenticatedIdentity;

/// Checks whether the authenticated identity is authorized to perform the given
/// action on the given resource.
///
/// Returns `Ok(())` if authorized, or an `S3Error::AccessDenied` if not.
///
/// Root users always pass (short-circuit). For other users, the preloaded
/// effective policies are evaluated using deny-overrides semantics.
pub fn authorize(
    identity: &AuthenticatedIdentity,
    action: &str,
    resource: &str,
) -> Result<(), S3Error> {
    if identity.is_root() {
        return Ok(());
    }

    match policy::evaluate_grants(&identity.effective_policies, action, resource) {
        Evaluation::Allow => Ok(()),
        Evaluation::Deny | Evaluation::NoMatch => Err(S3Error::new(
            S3ErrorCode::AccessDenied,
            resource,
        )),
    }
}

/// Builds an S3 resource ARN for a bucket.
///
/// Example: `arn:aws:s3:::my-bucket`
pub fn bucket_arn(bucket: &str) -> String {
    format!("arn:aws:s3:::{}", bucket)
}

/// Builds an S3 resource ARN for an object.
///
/// Example: `arn:aws:s3:::my-bucket/path/to/key`
pub fn object_arn(bucket: &str, key: &str) -> String {
    format!("arn:aws:s3:::{}/{}", bucket, key)
}
