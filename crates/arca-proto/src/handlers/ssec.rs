//! SSE-C (Server-Side Encryption with Customer-provided keys) header extraction.

use base64::Engine;
use http::HeaderMap;
use md5::{Digest, Md5};

use arca_core::{S3Error, S3ErrorCode};

/// Extracted SSE-C key from request headers.
pub struct SsecKey {
    /// The 32-byte customer-provided key.
    pub key: [u8; 32],
    /// The MD5 of the key (returned in response headers).
    pub key_md5: String,
}

/// Extracts SSE-C key from standard request headers.
///
/// Headers: `x-amz-server-side-encryption-customer-algorithm`,
///          `x-amz-server-side-encryption-customer-key`,
///          `x-amz-server-side-encryption-customer-key-md5`
///
/// Returns `None` if no SSE-C headers are present.
/// Returns `Err` on malformed headers (wrong algorithm, bad key, MD5 mismatch).
pub fn extract_ssec_key(
    headers: &HeaderMap,
    resource: &str,
) -> Result<Option<SsecKey>, S3Error> {
    let algo = headers
        .get("x-amz-server-side-encryption-customer-algorithm")
        .and_then(|v| v.to_str().ok());

    let key_b64 = headers
        .get("x-amz-server-side-encryption-customer-key")
        .and_then(|v| v.to_str().ok());

    let key_md5 = headers
        .get("x-amz-server-side-encryption-customer-key-md5")
        .and_then(|v| v.to_str().ok());

    // If none of the SSE-C headers are present, return None.
    if algo.is_none() && key_b64.is_none() && key_md5.is_none() {
        return Ok(None);
    }

    // If any SSE-C header is present, all must be present.
    let algo = algo.ok_or_else(|| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "x-amz-server-side-encryption-customer-algorithm is required with SSE-C",
            resource,
        )
    })?;

    if algo != "AES256" {
        return Err(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!("Invalid SSE-C algorithm: {algo}. Must be AES256."),
            resource,
        ));
    }

    let key_b64 = key_b64.ok_or_else(|| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "x-amz-server-side-encryption-customer-key is required with SSE-C",
            resource,
        )
    })?;

    let key_md5_header = key_md5.ok_or_else(|| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "x-amz-server-side-encryption-customer-key-md5 is required with SSE-C",
            resource,
        )
    })?;

    // Decode the key from base64.
    let b64 = &base64::engine::general_purpose::STANDARD;
    let key_bytes = b64.decode(key_b64).map_err(|_| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "Invalid base64 in x-amz-server-side-encryption-customer-key",
            resource,
        )
    })?;

    if key_bytes.len() != 32 {
        return Err(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!(
                "SSE-C key must be 256 bits (32 bytes), got {} bytes",
                key_bytes.len()
            ),
            resource,
        ));
    }

    // Verify the MD5 of the key matches the provided MD5.
    let computed_md5 = b64.encode(Md5::digest(&key_bytes));
    if computed_md5 != key_md5_header {
        return Err(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "SSE-C key MD5 does not match the provided key",
            resource,
        ));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes);

    Ok(Some(SsecKey {
        key,
        key_md5: key_md5_header.to_string(),
    }))
}

/// Extracts SSE-C key from copy-source headers.
///
/// Headers: `x-amz-copy-source-server-side-encryption-customer-algorithm`,
///          `x-amz-copy-source-server-side-encryption-customer-key`,
///          `x-amz-copy-source-server-side-encryption-customer-key-md5`
pub fn extract_ssec_copy_source_key(
    headers: &HeaderMap,
    resource: &str,
) -> Result<Option<SsecKey>, S3Error> {
    let algo = headers
        .get("x-amz-copy-source-server-side-encryption-customer-algorithm")
        .and_then(|v| v.to_str().ok());

    let key_b64 = headers
        .get("x-amz-copy-source-server-side-encryption-customer-key")
        .and_then(|v| v.to_str().ok());

    let key_md5 = headers
        .get("x-amz-copy-source-server-side-encryption-customer-key-md5")
        .and_then(|v| v.to_str().ok());

    if algo.is_none() && key_b64.is_none() && key_md5.is_none() {
        return Ok(None);
    }

    let algo = algo.ok_or_else(|| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "x-amz-copy-source-server-side-encryption-customer-algorithm is required",
            resource,
        )
    })?;

    if algo != "AES256" {
        return Err(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!("Invalid SSE-C copy source algorithm: {algo}. Must be AES256."),
            resource,
        ));
    }

    let key_b64 = key_b64.ok_or_else(|| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "x-amz-copy-source-server-side-encryption-customer-key is required",
            resource,
        )
    })?;

    let key_md5_header = key_md5.ok_or_else(|| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "x-amz-copy-source-server-side-encryption-customer-key-md5 is required",
            resource,
        )
    })?;

    let b64 = &base64::engine::general_purpose::STANDARD;
    let key_bytes = b64.decode(key_b64).map_err(|_| {
        S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "Invalid base64 in copy-source SSE-C key",
            resource,
        )
    })?;

    if key_bytes.len() != 32 {
        return Err(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            format!(
                "SSE-C copy source key must be 256 bits (32 bytes), got {} bytes",
                key_bytes.len()
            ),
            resource,
        ));
    }

    let computed_md5 = b64.encode(Md5::digest(&key_bytes));
    if computed_md5 != key_md5_header {
        return Err(S3Error::with_message(
            S3ErrorCode::InvalidArgument,
            "SSE-C copy source key MD5 does not match the provided key",
            resource,
        ));
    }

    let mut key = [0u8; 32];
    key.copy_from_slice(&key_bytes);

    Ok(Some(SsecKey {
        key,
        key_md5: key_md5_header.to_string(),
    }))
}
