//! S3 bucket name validation.
//!
//! Implements the S3 bucket naming rules:
//! <https://docs.aws.amazon.com/AmazonS3/latest/userguide/bucketnamingrules.html>

use crate::{S3Error, S3ErrorCode};

/// Validates an S3 bucket name according to the S3 naming rules.
///
/// Rules:
/// - 3–63 characters long
/// - Lowercase letters, digits, hyphens, and dots only
/// - Must start and end with a letter or digit
/// - No consecutive dots (`..`)
/// - Must not be formatted as an IP address (e.g. `192.168.1.1`)
/// - Must not start with `xn--` (internationalized domain prefix)
/// - Must not end with `-s3alias` or `--ol-s3`
pub fn validate_bucket_name(name: &str) -> Result<(), S3Error> {
    let make_err = || S3Error::with_message(S3ErrorCode::InvalidBucketName, format!("Invalid bucket name: {name}"), format!("/{name}"));

    let len = name.len();

    // Length check: 3–63 characters
    if len < 3 || len > 63 {
        return Err(make_err());
    }

    let bytes = name.as_bytes();

    // Must start with a lowercase letter or digit
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return Err(make_err());
    }

    // Must end with a lowercase letter or digit
    if !bytes[len - 1].is_ascii_lowercase() && !bytes[len - 1].is_ascii_digit() {
        return Err(make_err());
    }

    // Only lowercase letters, digits, hyphens, and dots
    for &b in bytes {
        if !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-' && b != b'.' {
            return Err(make_err());
        }
    }

    // No consecutive dots
    if name.contains("..") {
        return Err(make_err());
    }

    // Must not look like an IP address (digits and dots only, four octets)
    if looks_like_ip(name) {
        return Err(make_err());
    }

    // Must not start with xn--
    if name.starts_with("xn--") {
        return Err(make_err());
    }

    // Must not end with -s3alias or --ol-s3
    if name.ends_with("-s3alias") || name.ends_with("--ol-s3") {
        return Err(make_err());
    }

    Ok(())
}

/// Returns true if the name looks like an IPv4 address (four decimal octets).
fn looks_like_ip(name: &str) -> bool {
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts.iter().all(|p| !p.is_empty() && p.len() <= 3 && p.bytes().all(|b| b.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_simple_name() {
        assert!(validate_bucket_name("my-bucket").is_ok());
    }

    #[test]
    fn valid_with_dots() {
        assert!(validate_bucket_name("my.bucket.name").is_ok());
    }

    #[test]
    fn valid_all_digits() {
        assert!(validate_bucket_name("123").is_ok());
    }

    #[test]
    fn valid_min_length() {
        assert!(validate_bucket_name("abc").is_ok());
    }

    #[test]
    fn valid_max_length() {
        let name = "a".repeat(63);
        assert!(validate_bucket_name(&name).is_ok());
    }

    #[test]
    fn invalid_too_short() {
        assert!(validate_bucket_name("ab").is_err());
    }

    #[test]
    fn invalid_too_long() {
        let name = "a".repeat(64);
        assert!(validate_bucket_name(&name).is_err());
    }

    #[test]
    fn invalid_uppercase() {
        assert!(validate_bucket_name("My-Bucket").is_err());
    }

    #[test]
    fn invalid_underscore() {
        assert!(validate_bucket_name("my_bucket").is_err());
    }

    #[test]
    fn invalid_consecutive_dots() {
        assert!(validate_bucket_name("my..bucket").is_err());
    }

    #[test]
    fn invalid_starts_with_hyphen() {
        assert!(validate_bucket_name("-my-bucket").is_err());
    }

    #[test]
    fn invalid_ends_with_hyphen() {
        assert!(validate_bucket_name("my-bucket-").is_err());
    }

    #[test]
    fn invalid_starts_with_dot() {
        assert!(validate_bucket_name(".my-bucket").is_err());
    }

    #[test]
    fn invalid_ends_with_dot() {
        assert!(validate_bucket_name("my-bucket.").is_err());
    }

    #[test]
    fn invalid_ip_address() {
        assert!(validate_bucket_name("192.168.1.1").is_err());
    }

    #[test]
    fn invalid_xn_prefix() {
        assert!(validate_bucket_name("xn--something").is_err());
    }

    #[test]
    fn invalid_s3alias_suffix() {
        assert!(validate_bucket_name("bucket-s3alias").is_err());
    }

    #[test]
    fn invalid_ol_s3_suffix() {
        assert!(validate_bucket_name("bucket--ol-s3").is_err());
    }

    #[test]
    fn invalid_special_chars() {
        assert!(validate_bucket_name("my@bucket").is_err());
        assert!(validate_bucket_name("my bucket").is_err());
        assert!(validate_bucket_name("my/bucket").is_err());
    }

    #[test]
    fn valid_not_ip_with_letters() {
        // Has dots and digits but also letters — not an IP
        assert!(validate_bucket_name("192.168.1.bucket").is_ok());
    }

    #[test]
    fn error_has_correct_code() {
        let err = validate_bucket_name("AB").unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidBucketName);
    }
}
