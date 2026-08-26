//! ETag comparison for S3 conditional requests (`If-Match` / `If-None-Match`).
//!
//! Shared by `arca-proto` (the handler-side early-reject check) and
//! `arca-storage` (the authoritative check inside the write transaction) so
//! the two sides of the same conditional-write contract cannot drift apart.

/// Returns true if `header_val` — a raw `If-Match` / `If-None-Match` header
/// value, which may be `*` or a comma-separated list of quoted or unquoted
/// ETags — matches `etag` (a quoted ETag, e.g. `"abc123"`).
pub fn etag_matches(header_val: &str, etag: &str) -> bool {
    let trimmed = header_val.trim();
    if trimmed == "*" {
        return true;
    }
    trimmed
        .split(',')
        .any(|v| v.trim().trim_matches('"') == etag.trim_matches('"'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn star_matches_anything() {
        assert!(etag_matches("*", "\"abc123\""));
        assert!(etag_matches(" * ", "\"abc123\""));
    }

    #[test]
    fn quoted_etag_matches() {
        assert!(etag_matches("\"abc123\"", "\"abc123\""));
    }

    #[test]
    fn unquoted_etag_matches_quoted() {
        assert!(etag_matches("abc123", "\"abc123\""));
    }

    #[test]
    fn mismatched_etag_does_not_match() {
        assert!(!etag_matches("\"other\"", "\"abc123\""));
    }

    #[test]
    fn comma_separated_list_matches_any() {
        assert!(etag_matches("\"foo\", \"abc123\", \"bar\"", "\"abc123\""));
        assert!(!etag_matches("\"foo\", \"bar\"", "\"abc123\""));
    }

    #[test]
    fn whitespace_around_values_is_trimmed() {
        assert!(etag_matches(" \"abc123\" ", "\"abc123\""));
    }
}
