//! AWS SigV4 Authorization header and query-string auth parser.

use crate::error::AuthError;

/// Parsed fields from an `Authorization: AWS4-HMAC-SHA256 ...` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAuthorization {
    pub access_key_id: String,
    /// Date component from credential scope (YYYYMMDD).
    pub date: String,
    pub region: String,
    pub service: String,
    /// Signed header names, lowercase and sorted.
    pub signed_headers: Vec<String>,
    /// The hex-encoded signature.
    pub signature: String,
}

/// Parses an `Authorization` header value in AWS SigV4 format.
///
/// Expected format:
/// ```text
/// AWS4-HMAC-SHA256 Credential=KEY/DATE/REGION/SERVICE/aws4_request, SignedHeaders=h1;h2, Signature=HEX
/// ```
pub fn parse_authorization(header: &str) -> Result<ParsedAuthorization, AuthError> {
    let header = header.trim();

    // Must start with the algorithm prefix
    let rest = header
        .strip_prefix("AWS4-HMAC-SHA256")
        .ok_or_else(|| AuthError::MalformedHeader("missing AWS4-HMAC-SHA256 algorithm".into()))?;

    let rest = rest.trim_start();

    // Parse key=value pairs separated by ", "
    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;

    for part in rest.split(',') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("Credential=") {
            credential = Some(val.trim());
        } else if let Some(val) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(val.trim());
        } else if let Some(val) = part.strip_prefix("Signature=") {
            signature = Some(val.trim());
        }
    }

    // Credential
    let credential = credential
        .ok_or_else(|| AuthError::MalformedHeader("missing Credential".into()))?;
    let cred_parts: Vec<&str> = credential.split('/').collect();
    if cred_parts.len() != 5 {
        return Err(AuthError::MalformedHeader(format!(
            "credential scope must have 5 parts, got {}",
            cred_parts.len()
        )));
    }
    if cred_parts[4] != "aws4_request" {
        return Err(AuthError::MalformedHeader(
            "credential scope must end with aws4_request".into(),
        ));
    }

    // SignedHeaders
    let signed_headers_str = signed_headers
        .ok_or_else(|| AuthError::MalformedHeader("missing SignedHeaders".into()))?;
    let signed_headers: Vec<String> = signed_headers_str
        .split(';')
        .map(|s| s.trim().to_lowercase())
        .collect();
    if signed_headers.is_empty() {
        return Err(AuthError::MalformedHeader("empty SignedHeaders".into()));
    }

    // Signature
    let signature = signature
        .ok_or_else(|| AuthError::MalformedHeader("missing Signature".into()))?;
    if signature.is_empty() {
        return Err(AuthError::MalformedHeader("empty Signature".into()));
    }

    Ok(ParsedAuthorization {
        access_key_id: cred_parts[0].to_string(),
        date: cred_parts[1].to_string(),
        region: cred_parts[2].to_string(),
        service: cred_parts[3].to_string(),
        signed_headers,
        signature: signature.to_string(),
    })
}

/// Parsed fields from query-string auth parameters (presigned URL).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQueryAuth {
    pub access_key_id: String,
    /// Date component from credential scope (YYYYMMDD).
    pub date: String,
    pub region: String,
    pub service: String,
    /// Signed header names, lowercase and sorted.
    pub signed_headers: Vec<String>,
    /// The hex-encoded signature.
    pub signature: String,
    /// X-Amz-Expires value in seconds.
    pub expires: u64,
    /// X-Amz-Date value (ISO 8601: 20130524T000000Z).
    pub request_datetime: String,
}

/// Parses query-string authentication parameters from a URL query string.
///
/// Expected query params:
/// - `X-Amz-Algorithm=AWS4-HMAC-SHA256`
/// - `X-Amz-Credential=KEY/DATE/REGION/SERVICE/aws4_request`
/// - `X-Amz-Date=20130524T000000Z`
/// - `X-Amz-Expires=3600`
/// - `X-Amz-SignedHeaders=host`
/// - `X-Amz-Signature=HEX`
pub fn parse_query_string_auth(query: &str) -> Result<ParsedQueryAuth, AuthError> {
    let mut algorithm = None;
    let mut credential = None;
    let mut date = None;
    let mut expires = None;
    let mut signed_headers = None;
    let mut signature = None;

    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "X-Amz-Algorithm" => algorithm = Some(value.to_string()),
            "X-Amz-Credential" => credential = Some(value.to_string()),
            "X-Amz-Date" => date = Some(value.to_string()),
            "X-Amz-Expires" => expires = Some(value.to_string()),
            "X-Amz-SignedHeaders" => signed_headers = Some(value.to_string()),
            "X-Amz-Signature" => signature = Some(value.to_string()),
            _ => {}
        }
    }

    // Validate algorithm
    let algo = algorithm
        .ok_or_else(|| AuthError::MalformedQueryAuth("missing X-Amz-Algorithm".into()))?;
    if algo != "AWS4-HMAC-SHA256" {
        return Err(AuthError::MalformedQueryAuth(format!(
            "unsupported algorithm: {algo}"
        )));
    }

    // Parse credential scope
    let credential = credential
        .ok_or_else(|| AuthError::MalformedQueryAuth("missing X-Amz-Credential".into()))?;
    let cred_parts: Vec<&str> = credential.split('/').collect();
    if cred_parts.len() != 5 {
        return Err(AuthError::MalformedQueryAuth(format!(
            "credential scope must have 5 parts, got {}",
            cred_parts.len()
        )));
    }
    if cred_parts[4] != "aws4_request" {
        return Err(AuthError::MalformedQueryAuth(
            "credential scope must end with aws4_request".into(),
        ));
    }

    // Parse date
    let request_datetime = date
        .ok_or_else(|| AuthError::MalformedQueryAuth("missing X-Amz-Date".into()))?;
    if request_datetime.is_empty() {
        return Err(AuthError::MalformedQueryAuth("empty X-Amz-Date".into()));
    }

    // Parse expires
    let expires_str = expires
        .ok_or_else(|| AuthError::MalformedQueryAuth("missing X-Amz-Expires".into()))?;
    let expires_val: u64 = expires_str.parse().map_err(|_| {
        AuthError::MalformedQueryAuth(format!("invalid X-Amz-Expires: {expires_str}"))
    })?;

    // Parse signed headers
    let signed_headers_str = signed_headers
        .ok_or_else(|| AuthError::MalformedQueryAuth("missing X-Amz-SignedHeaders".into()))?;
    let signed_headers: Vec<String> = signed_headers_str
        .split(';')
        .map(|s| s.trim().to_lowercase())
        .collect();
    if signed_headers.is_empty() {
        return Err(AuthError::MalformedQueryAuth(
            "empty X-Amz-SignedHeaders".into(),
        ));
    }

    // Parse signature
    let signature = signature
        .ok_or_else(|| AuthError::MalformedQueryAuth("missing X-Amz-Signature".into()))?;
    if signature.is_empty() {
        return Err(AuthError::MalformedQueryAuth("empty X-Amz-Signature".into()));
    }

    Ok(ParsedQueryAuth {
        access_key_id: cred_parts[0].to_string(),
        date: cred_parts[1].to_string(),
        region: cred_parts[2].to_string(),
        service: cred_parts[3].to_string(),
        signed_headers,
        signature,
        expires: expires_val,
        request_datetime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_header() {
        let header = "AWS4-HMAC-SHA256 \
            Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
            SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
            Signature=fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024";
        let parsed = parse_authorization(header).unwrap();
        assert_eq!(parsed.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(parsed.date, "20130524");
        assert_eq!(parsed.region, "us-east-1");
        assert_eq!(parsed.service, "s3");
        assert_eq!(
            parsed.signed_headers,
            vec!["host", "range", "x-amz-content-sha256", "x-amz-date"]
        );
        assert_eq!(
            parsed.signature,
            "fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024"
        );
    }

    #[test]
    fn parse_missing_algorithm() {
        let header = "Credential=KEY/20130524/us-east-1/s3/aws4_request, \
            SignedHeaders=host, Signature=abc123";
        let err = parse_authorization(header).unwrap_err();
        assert!(matches!(err, AuthError::MalformedHeader(_)));
    }

    #[test]
    fn parse_bad_credential_scope() {
        let header = "AWS4-HMAC-SHA256 \
            Credential=KEY/20130524/us-east-1/s3, \
            SignedHeaders=host, Signature=abc123";
        let err = parse_authorization(header).unwrap_err();
        assert!(matches!(err, AuthError::MalformedHeader(_)));
    }

    #[test]
    fn parse_missing_signature() {
        let header = "AWS4-HMAC-SHA256 \
            Credential=KEY/20130524/us-east-1/s3/aws4_request, \
            SignedHeaders=host";
        let err = parse_authorization(header).unwrap_err();
        assert!(matches!(err, AuthError::MalformedHeader(_)));
    }

    #[test]
    fn parse_missing_signed_headers() {
        let header = "AWS4-HMAC-SHA256 \
            Credential=KEY/20130524/us-east-1/s3/aws4_request, \
            Signature=abc123";
        let err = parse_authorization(header).unwrap_err();
        assert!(matches!(err, AuthError::MalformedHeader(_)));
    }

    #[test]
    fn parse_bad_aws4_request_suffix() {
        let header = "AWS4-HMAC-SHA256 \
            Credential=KEY/20130524/us-east-1/s3/bad_suffix, \
            SignedHeaders=host, Signature=abc123";
        let err = parse_authorization(header).unwrap_err();
        assert!(matches!(err, AuthError::MalformedHeader(_)));
    }

    #[test]
    fn parse_extra_whitespace() {
        let header = "AWS4-HMAC-SHA256   \
            Credential=KEY/20130524/us-east-1/s3/aws4_request,  \
            SignedHeaders=host;x-amz-date,  \
            Signature=abc123";
        let parsed = parse_authorization(header).unwrap();
        assert_eq!(parsed.access_key_id, "KEY");
        assert_eq!(parsed.signed_headers, vec!["host", "x-amz-date"]);
        assert_eq!(parsed.signature, "abc123");
    }

    // --- Query-string auth tests ---

    #[test]
    fn parse_query_auth_valid() {
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
            &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
            &X-Amz-Date=20130524T000000Z\
            &X-Amz-Expires=86400\
            &X-Amz-SignedHeaders=host\
            &X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404";
        let parsed = parse_query_string_auth(query).unwrap();
        assert_eq!(parsed.access_key_id, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(parsed.date, "20130524");
        assert_eq!(parsed.region, "us-east-1");
        assert_eq!(parsed.service, "s3");
        assert_eq!(parsed.signed_headers, vec!["host"]);
        assert_eq!(parsed.expires, 86400);
        assert_eq!(parsed.request_datetime, "20130524T000000Z");
        assert_eq!(
            parsed.signature,
            "aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
        );
    }

    #[test]
    fn parse_query_auth_missing_algorithm() {
        let query = "X-Amz-Credential=KEY%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
            &X-Amz-Date=20130524T000000Z\
            &X-Amz-Expires=3600\
            &X-Amz-SignedHeaders=host\
            &X-Amz-Signature=abc123";
        let err = parse_query_string_auth(query).unwrap_err();
        assert!(matches!(err, AuthError::MalformedQueryAuth(_)));
    }

    #[test]
    fn parse_query_auth_wrong_algorithm() {
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA512\
            &X-Amz-Credential=KEY%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
            &X-Amz-Date=20130524T000000Z\
            &X-Amz-Expires=3600\
            &X-Amz-SignedHeaders=host\
            &X-Amz-Signature=abc123";
        let err = parse_query_string_auth(query).unwrap_err();
        assert!(matches!(err, AuthError::MalformedQueryAuth(_)));
    }

    #[test]
    fn parse_query_auth_missing_signature() {
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
            &X-Amz-Credential=KEY%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
            &X-Amz-Date=20130524T000000Z\
            &X-Amz-Expires=3600\
            &X-Amz-SignedHeaders=host";
        let err = parse_query_string_auth(query).unwrap_err();
        assert!(matches!(err, AuthError::MalformedQueryAuth(_)));
    }

    #[test]
    fn parse_query_auth_bad_credential_scope() {
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
            &X-Amz-Credential=KEY%2F20130524%2Fus-east-1%2Fs3\
            &X-Amz-Date=20130524T000000Z\
            &X-Amz-Expires=3600\
            &X-Amz-SignedHeaders=host\
            &X-Amz-Signature=abc123";
        let err = parse_query_string_auth(query).unwrap_err();
        assert!(matches!(err, AuthError::MalformedQueryAuth(_)));
    }

    #[test]
    fn parse_query_auth_invalid_expires() {
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
            &X-Amz-Credential=KEY%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
            &X-Amz-Date=20130524T000000Z\
            &X-Amz-Expires=notanumber\
            &X-Amz-SignedHeaders=host\
            &X-Amz-Signature=abc123";
        let err = parse_query_string_auth(query).unwrap_err();
        assert!(matches!(err, AuthError::MalformedQueryAuth(_)));
    }

    #[test]
    fn parse_query_auth_missing_date() {
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
            &X-Amz-Credential=KEY%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
            &X-Amz-Expires=3600\
            &X-Amz-SignedHeaders=host\
            &X-Amz-Signature=abc123";
        let err = parse_query_string_auth(query).unwrap_err();
        assert!(matches!(err, AuthError::MalformedQueryAuth(_)));
    }

    #[test]
    fn parse_query_auth_credential_decoded() {
        // form_urlencoded::parse auto-decodes %2F in credential
        let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256\
            &X-Amz-Credential=AKID%2F20130524%2Fus-east-1%2Fs3%2Faws4_request\
            &X-Amz-Date=20130524T000000Z\
            &X-Amz-Expires=3600\
            &X-Amz-SignedHeaders=host\
            &X-Amz-Signature=abc123";
        let parsed = parse_query_string_auth(query).unwrap();
        assert_eq!(parsed.access_key_id, "AKID");
        assert_eq!(parsed.date, "20130524");
    }
}
