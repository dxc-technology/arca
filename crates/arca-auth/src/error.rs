//! Auth-specific error types.

use std::fmt;

/// Errors that can occur during AWS SigV4 authentication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    /// Authorization header is missing or malformed.
    MalformedHeader(String),
    /// Signature does not match the computed value.
    SignatureDoesNotMatch,
    /// Date/time format is invalid.
    InvalidDateTime(String),
    /// Presigned URL has expired.
    ExpiredUrl,
    /// Query-string auth parameters are missing or malformed.
    MalformedQueryAuth(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::MalformedHeader(msg) => write!(f, "malformed authorization header: {msg}"),
            AuthError::SignatureDoesNotMatch => write!(f, "signature does not match"),
            AuthError::InvalidDateTime(msg) => write!(f, "invalid date/time: {msg}"),
            AuthError::ExpiredUrl => write!(f, "presigned URL has expired"),
            AuthError::MalformedQueryAuth(msg) => write!(f, "malformed query auth: {msg}"),
        }
    }
}

impl std::error::Error for AuthError {}
