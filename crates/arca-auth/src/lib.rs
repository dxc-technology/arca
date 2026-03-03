//! arca-auth: AWS Signature Version 4 verification for Arca.
//!
//! This crate has zero I/O dependencies and is independently testable
//! against AWS test vectors.

mod error;
mod parse;
mod sigv4;

pub use error::AuthError;
pub use parse::{parse_authorization, ParsedAuthorization};
pub use sigv4::{verify_request, VerifyInput};
