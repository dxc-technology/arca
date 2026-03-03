//! AWS SigV4 Authorization header parser.

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
}
