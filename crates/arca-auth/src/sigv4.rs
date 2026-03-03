//! AWS Signature Version 4 computation and verification.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::AuthError;
use crate::parse::ParsedAuthorization;

type HmacSha256 = Hmac<Sha256>;

/// Input required to verify an AWS SigV4 request signature.
pub struct VerifyInput<'a> {
    /// HTTP method (GET, PUT, POST, DELETE, HEAD).
    pub method: &'a str,
    /// URI path (e.g. "/bucket/key"). Must be the raw path before any rewriting.
    pub uri_path: &'a str,
    /// Query string without leading '?' (e.g. "list-type=2&prefix=foo").
    pub query_string: &'a str,
    /// All request headers as (name, value) pairs.
    pub headers: &'a [(String, String)],
    /// Value of `x-amz-content-sha256` header (e.g. "UNSIGNED-PAYLOAD" or hex SHA256).
    pub payload_hash: &'a str,
    /// Parsed Authorization header.
    pub auth: &'a ParsedAuthorization,
    /// The secret access key for this credential.
    pub secret_access_key: &'a str,
    /// Value of `x-amz-date` header (ISO 8601: 20130524T000000Z).
    pub request_datetime: &'a str,
}

/// Verifies an AWS SigV4 request signature.
///
/// Returns `Ok(())` if the signature is valid, or an `AuthError` if not.
pub fn verify_request(input: &VerifyInput) -> Result<(), AuthError> {
    // 1. Build canonical request
    let canonical = canonical_request(
        input.method,
        input.uri_path,
        input.query_string,
        input.headers,
        &input.auth.signed_headers,
        input.payload_hash,
    );

    // 2. Hash the canonical request
    let canonical_hash = hex_sha256(canonical.as_bytes());

    // 3. Build the credential scope
    let scope = format!(
        "{}/{}/{}/aws4_request",
        input.auth.date, input.auth.region, input.auth.service
    );

    // 4. Build string to sign
    let sts = string_to_sign(input.request_datetime, &scope, &canonical_hash);

    // 5. Derive signing key
    let key = signing_key(
        input.secret_access_key,
        &input.auth.date,
        &input.auth.region,
        &input.auth.service,
    );

    // 6. Compute signature
    let computed = compute_signature(&key, &sts);

    // 7. Constant-time comparison
    let computed_bytes = computed.as_bytes();
    let provided_bytes = input.auth.signature.as_bytes();
    if computed_bytes.ct_eq(provided_bytes).into() {
        Ok(())
    } else {
        Err(AuthError::SignatureDoesNotMatch)
    }
}

/// URI-encode a path for canonical request (S3 exception: don't double-encode).
///
/// S3 uses the raw URI path as-is for the canonical URI. Unlike other AWS services,
/// S3 does NOT re-encode already-encoded characters. The path is taken verbatim
/// from the HTTP request line.
fn canonical_uri(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    path.to_string()
}

/// Build the canonical query string.
///
/// Parse query params, URI-encode keys and values, sort by key (then value), join with &.
fn canonical_query_string(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }

    let mut pairs: Vec<(String, String)> = form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (uri_encode(&k, true), uri_encode(&v, true)))
        .collect();

    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Build canonical headers string from signed header names and all headers.
///
/// - Lowercase header names
/// - Trim values, collapse consecutive spaces to single space
/// - Sort by header name
/// - Each line ends with `\n`
fn canonical_headers(
    all_headers: &[(String, String)],
    signed_header_names: &[String],
) -> String {
    let mut selected: Vec<(String, String)> = Vec::new();

    for name in signed_header_names {
        let lower_name = name.to_lowercase();
        // Collect all values for this header name
        for (hname, hval) in all_headers {
            if hname.to_lowercase() == lower_name {
                let trimmed = trim_header_value(hval);
                selected.push((lower_name.clone(), trimmed));
            }
        }
    }

    // Sort by name (already should be in order from signed_headers, but sort to be safe)
    selected.sort_by(|a, b| a.0.cmp(&b.0));

    let mut result = String::new();
    for (name, value) in &selected {
        result.push_str(name);
        result.push(':');
        result.push_str(value);
        result.push('\n');
    }
    result
}

/// Trim a header value: strip leading/trailing whitespace, collapse consecutive spaces.
fn trim_header_value(value: &str) -> String {
    let trimmed = value.trim();
    let mut result = String::with_capacity(trimmed.len());
    let mut prev_space = false;
    for c in trimmed.chars() {
        if c == ' ' {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result
}

/// Build the semicolon-joined signed headers string.
fn signed_headers_str(headers: &[String]) -> String {
    let mut sorted = headers.to_vec();
    sorted.sort();
    sorted.join(";")
}

/// Assemble the canonical request string.
fn canonical_request(
    method: &str,
    uri_path: &str,
    query_string: &str,
    all_headers: &[(String, String)],
    signed_header_names: &[String],
    payload_hash: &str,
) -> String {
    let cu = canonical_uri(uri_path);
    let cq = canonical_query_string(query_string);
    let ch = canonical_headers(all_headers, signed_header_names);
    let sh = signed_headers_str(signed_header_names);

    format!("{method}\n{cu}\n{cq}\n{ch}\n{sh}\n{payload_hash}")
}

/// Build the "string to sign".
fn string_to_sign(datetime: &str, scope: &str, canonical_request_hash: &str) -> String {
    format!("AWS4-HMAC-SHA256\n{datetime}\n{scope}\n{canonical_request_hash}")
}

/// Derive the SigV4 signing key via 4-step HMAC-SHA256 chain.
fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_secret = format!("AWS4{secret}");
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Compute the final signature as a hex string.
fn compute_signature(signing_key: &[u8], string_to_sign: &str) -> String {
    let sig = hmac_sha256(signing_key, string_to_sign.as_bytes());
    hex::encode(sig)
}

/// Compute HMAC-SHA256.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac =
        HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// Compute SHA256 and return hex string.
fn hex_sha256(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// URI-encode a string per AWS SigV4 rules.
///
/// Unreserved characters (A-Z, a-z, 0-9, '-', '.', '_', '~') are not encoded.
/// If `encode_slash` is true, '/' is encoded as %2F; otherwise preserved.
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut result = String::with_capacity(input.len() * 2);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                result.push(byte as char);
            }
            b'/' if !encode_slash => {
                result.push('/');
            }
            _ => {
                result.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_authorization;

    // --- canonical_uri tests ---

    #[test]
    fn canonical_uri_root() {
        assert_eq!(canonical_uri("/"), "/");
    }

    #[test]
    fn canonical_uri_basic_path() {
        assert_eq!(canonical_uri("/bucket/key"), "/bucket/key");
    }

    #[test]
    fn canonical_uri_already_encoded() {
        // S3 does not double-encode: path is used verbatim
        assert_eq!(
            canonical_uri("/bucket/test%24file.text"),
            "/bucket/test%24file.text"
        );
    }

    #[test]
    fn canonical_uri_empty() {
        assert_eq!(canonical_uri(""), "/");
    }

    // --- canonical_query_string tests ---

    #[test]
    fn canonical_query_string_sorting() {
        assert_eq!(
            canonical_query_string("b=2&a=1"),
            "a=1&b=2"
        );
    }

    #[test]
    fn canonical_query_string_encoding() {
        assert_eq!(
            canonical_query_string("key=hello world"),
            "key=hello%20world"
        );
    }

    #[test]
    fn canonical_query_string_empty() {
        assert_eq!(canonical_query_string(""), "");
    }

    // --- canonical_headers tests ---

    #[test]
    fn canonical_headers_trimming_and_sorting() {
        let headers = vec![
            ("X-Amz-Date".to_string(), "20130524T000000Z".to_string()),
            ("Host".to_string(), "  example.com  ".to_string()),
        ];
        let signed = vec!["host".to_string(), "x-amz-date".to_string()];
        let result = canonical_headers(&headers, &signed);
        assert_eq!(
            result,
            "host:example.com\nx-amz-date:20130524T000000Z\n"
        );
    }

    #[test]
    fn canonical_headers_space_collapsing() {
        let headers = vec![
            ("My-Header".to_string(), "a   b   c".to_string()),
        ];
        let signed = vec!["my-header".to_string()];
        let result = canonical_headers(&headers, &signed);
        assert_eq!(result, "my-header:a b c\n");
    }

    // --- signing_key tests ---

    #[test]
    fn signing_key_aws_example() {
        // From AWS documentation example
        let key = signing_key(
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "iam",
        );
        // AWS documented expected value for this input
        assert_eq!(
            hex::encode(&key),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    // --- uri_encode tests ---

    #[test]
    fn uri_encode_unreserved() {
        assert_eq!(uri_encode("abcABC123-._~", true), "abcABC123-._~");
    }

    #[test]
    fn uri_encode_slash() {
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("a/b", false), "a/b");
    }

    #[test]
    fn uri_encode_space() {
        assert_eq!(uri_encode("hello world", true), "hello%20world");
    }

    // --- Full verify_request test using AWS example ---

    #[test]
    fn verify_request_aws_get_example() {
        // From AWS SigV4 S3 documentation: "Example: GET Object"
        let auth_header = "AWS4-HMAC-SHA256 \
            Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
            SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
            Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41";
        let auth = parse_authorization(auth_header).unwrap();

        let headers = vec![
            ("Host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("Range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];

        let input = VerifyInput {
            method: "GET",
            uri_path: "/test.txt",
            query_string: "",
            headers: &headers,
            payload_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            auth: &auth,
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            request_datetime: "20130524T000000Z",
        };

        assert!(verify_request(&input).is_ok());
    }

    #[test]
    fn verify_request_wrong_secret() {
        let auth_header = "AWS4-HMAC-SHA256 \
            Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
            SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
            Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41";
        let auth = parse_authorization(auth_header).unwrap();

        let headers = vec![
            ("Host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("Range".to_string(), "bytes=0-9".to_string()),
            ("x-amz-content-sha256".to_string(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];

        let input = VerifyInput {
            method: "GET",
            uri_path: "/test.txt",
            query_string: "",
            headers: &headers,
            payload_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            auth: &auth,
            secret_access_key: "WRONG_SECRET_KEY",
            request_datetime: "20130524T000000Z",
        };

        assert!(matches!(
            verify_request(&input),
            Err(AuthError::SignatureDoesNotMatch)
        ));
    }

    #[test]
    fn verify_request_put_object() {
        // PUT /test$file.text with date header
        // From AWS docs: "PUT Object" example
        let auth_header = "AWS4-HMAC-SHA256 \
            Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
            SignedHeaders=date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class, \
            Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd";
        let auth = parse_authorization(auth_header).unwrap();

        let headers = vec![
            ("Date".to_string(), "Fri, 24 May 2013 00:00:00 GMT".to_string()),
            ("Host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("x-amz-content-sha256".to_string(), "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072".to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
            ("x-amz-storage-class".to_string(), "REDUCED_REDUNDANCY".to_string()),
        ];

        let input = VerifyInput {
            method: "PUT",
            uri_path: "/test%24file.text",
            query_string: "",
            headers: &headers,
            payload_hash: "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072",
            auth: &auth,
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            request_datetime: "20130524T000000Z",
        };

        assert!(verify_request(&input).is_ok());
    }

    #[test]
    fn verify_request_get_with_query_params() {
        // GET /?lifecycle from AWS docs
        let auth_header = "AWS4-HMAC-SHA256 \
            Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
            SignedHeaders=host;x-amz-content-sha256;x-amz-date, \
            Signature=fea454ca298b7da1c68078a5d1bdbfbbe0d65c699e0f91ac7a200a0136783543";
        let auth = parse_authorization(auth_header).unwrap();

        let headers = vec![
            ("Host".to_string(), "examplebucket.s3.amazonaws.com".to_string()),
            ("x-amz-content-sha256".to_string(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string()),
            ("x-amz-date".to_string(), "20130524T000000Z".to_string()),
        ];

        let input = VerifyInput {
            method: "GET",
            uri_path: "/",
            query_string: "lifecycle",
            headers: &headers,
            payload_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            auth: &auth,
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            request_datetime: "20130524T000000Z",
        };

        assert!(verify_request(&input).is_ok());
    }

    #[test]
    fn hex_sha256_empty() {
        // SHA256("") = known value
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
