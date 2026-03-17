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
