//! Authenticated identity types shared between S3 and admin auth middlewares.

use arca_core::policy::PolicyDocument;
use arca_core::types::{Credential, User};

/// Authenticated identity stored in request extensions after successful auth.
///
/// Contains the credential, the owning user, and preloaded effective policies
/// for non-root users (so handlers can do pure, synchronous authorization checks).
#[derive(Debug, Clone)]
pub struct AuthenticatedIdentity {
    pub credential: Credential,
    pub user: User,
    /// Preloaded effective policy documents (direct + team grants).
    /// Empty for root users (they bypass policy evaluation).
    pub effective_policies: Vec<PolicyDocument>,
}

impl AuthenticatedIdentity {
    /// Returns true if this identity belongs to the root user.
    pub fn is_root(&self) -> bool {
        self.user.is_root
    }

    /// Returns the username of the authenticated user.
    pub fn username(&self) -> &str {
        &self.user.username
    }
}

/// Username of the authenticated requester, the owner recorded on the rows
/// the request creates. The S3 router always runs `auth_middleware`, so the
/// fallback only covers code paths built without it (it matches ListBuckets).
pub fn requester_username(extensions: &http::Extensions) -> String {
    extensions
        .get::<AuthenticatedIdentity>()
        .map(|id| id.username().to_string())
        .unwrap_or_else(|| "root".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn identity(username: &str) -> AuthenticatedIdentity {
        AuthenticatedIdentity {
            credential: Credential {
                access_key_id: "AKIATEST".to_string(),
                secret_access_key: "secret".to_string(),
                description: String::new(),
                created_at: Utc::now(),
                active: true,
                user_id: "u-1".to_string(),
            },
            user: User {
                user_id: "u-1".to_string(),
                username: username.to_string(),
                description: String::new(),
                is_root: false,
                created_at: Utc::now(),
            },
            effective_policies: Vec::new(),
        }
    }

    #[test]
    fn requester_username_is_the_authenticated_users_name() {
        let mut extensions = http::Extensions::new();
        extensions.insert(identity("alice"));
        assert_eq!(requester_username(&extensions), "alice");
    }

    #[test]
    fn requester_username_falls_back_to_root_without_identity() {
        let extensions = http::Extensions::new();
        assert_eq!(requester_username(&extensions), "root");
    }
}
