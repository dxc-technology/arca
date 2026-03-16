//! arca-auth: AWS Signature Version 4 verification for Arca.
//!
//! This crate has zero I/O dependencies and is independently testable
//! against AWS test vectors.

mod error;
mod parse;
mod sigv4;

pub use error::AuthError;
pub use parse::{parse_authorization, parse_query_string_auth, ParsedAuthorization, ParsedQueryAuth};
pub use sigv4::{generate_presigned_url, verify_presigned_request, verify_request, PresignedVerifyInput, VerifyInput};
