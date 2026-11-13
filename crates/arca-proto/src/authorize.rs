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

/// Whether the identity the auth middleware stored in `extensions` may perform
/// `action` on `resource`.
///
/// For handlers that act on resources other than the one the middleware
/// authorized from the request path (a copy source, the keys of a batch or an
/// archive, a governance bypass). Fails closed: a request that carries no
/// identity is never authorized.
pub fn is_authorized(extensions: &http::Extensions, action: &str, resource: &str) -> bool {
    extensions
        .get::<AuthenticatedIdentity>()
        .is_some_and(|identity| authorize(identity, action, resource).is_ok())
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

#[cfg(test)]
mod tests {
    use arca_core::policy::PolicyDocument;
    use arca_core::types::{Credential, User};

    use super::*;

    fn identity(is_root: bool, policy: &str) -> AuthenticatedIdentity {
        let now = chrono::Utc::now();
        AuthenticatedIdentity {
            credential: Credential {
                access_key_id: "AKIATEST".to_string(),
                secret_access_key: "secret".to_string(),
                description: String::new(),
                created_at: now,
                active: true,
                user_id: "u".to_string(),
            },
            user: User {
                user_id: "u".to_string(),
                username: "u".to_string(),
                description: String::new(),
                is_root,
                created_at: now,
            },
            effective_policies: if policy.is_empty() {
                Vec::new()
            } else {
                vec![serde_json::from_str::<PolicyDocument>(policy).unwrap()]
            },
        }
    }

    fn extensions_with(identity: AuthenticatedIdentity) -> http::Extensions {
        let mut extensions = http::Extensions::new();
        extensions.insert(identity);
        extensions
    }

    const READ_PUBLIC: &str = r#"{"Version":"2012-10-17","Statement":[
        {"Effect":"Allow","Action":["s3:GetObject"],"Resource":["arn:aws:s3:::b/public/*"]}]}"#;

    #[test]
    fn a_request_without_identity_is_never_authorized() {
        let extensions = http::Extensions::new();
        assert!(!is_authorized(&extensions, "s3:GetObject", &object_arn("b", "k")));
    }

    #[test]
    fn root_is_authorized_without_policies() {
        let extensions = extensions_with(identity(true, ""));
        assert!(is_authorized(&extensions, "s3:GetObject", &object_arn("b", "k")));
    }

    #[test]
    fn a_user_is_authorized_only_where_a_grant_allows() {
        let extensions = extensions_with(identity(false, READ_PUBLIC));
        assert!(is_authorized(&extensions, "s3:GetObject", &object_arn("b", "public/a")));
        assert!(!is_authorized(&extensions, "s3:GetObject", &object_arn("b", "private/a")));
        assert!(!is_authorized(&extensions, "s3:DeleteObject", &object_arn("b", "public/a")));
    }

    #[test]
    fn a_user_without_grants_is_not_authorized() {
        let extensions = extensions_with(identity(false, ""));
        assert!(!is_authorized(&extensions, "s3:GetObject", &object_arn("b", "k")));
    }
}
