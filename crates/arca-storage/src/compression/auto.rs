//! Deterministic rule table that picks a concrete compression algorithm
//! (and level) when the operator chose `auto`.
//!
//! No sampling, no benchmarking, no ML. A plain match statement.
//! Evaluated in order — first match wins.

use arca_core::store::CompressionAlgorithm;

/// Web-asset content types that compress unusually well with Brotli.
const WEB_ASSETS: &[&str] = &[
    "application/javascript",
    "text/javascript",
    "application/ecmascript",
    "text/html",
    "text/css",
    "image/svg+xml",
    "application/xhtml+xml",
];

/// Structured text formats where Zstd gives the best ratio/speed balance.
const STRUCTURED_TEXT: &[&str] = &[
    "application/json",
    "application/xml",
    "application/yaml",
    "application/x-yaml",
    "application/x-ndjson",
    "application/x-log",
    "application/graphql",
];

/// Threshold below which small objects use Lz4 (CPU overhead dominates).
const SMALL_OBJECT_BYTES: u64 = 4 * 1024;

/// Returns a concrete `(algorithm, level)` for `auto` mode.
pub fn pick_auto(content_type: Option<&str>, size_hint: Option<u64>) -> (CompressionAlgorithm, i32) {
    // Rule 1: small objects prefer Lz4 (default level).
    if let Some(size) = size_hint {
        if size < SMALL_OBJECT_BYTES {
            return (CompressionAlgorithm::Lz4, 0);
        }
    }

    // Match the content type case-insensitively, ignoring parameters
    // such as "; charset=utf-8".
    let base_ct = content_type
        .map(|ct| ct.split(';').next().unwrap_or(ct).trim().to_ascii_lowercase());

    if let Some(ct) = &base_ct {
        // Rule 2: known web assets → Brotli quality 4.
        if WEB_ASSETS.iter().any(|w| *w == ct) {
            return (CompressionAlgorithm::Brotli, 4);
        }
        // Rule 3: structured text → Zstd level 3.
        if STRUCTURED_TEXT.iter().any(|w| *w == ct) || ct.starts_with("text/") {
            return (CompressionAlgorithm::Zstd, 3);
        }
    }

    // Rule 4: fallback — Zstd level 3.
    (CompressionAlgorithm::Zstd, 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiny_object_picks_lz4() {
        let (alg, _) = pick_auto(Some("application/json"), Some(1024));
        assert_eq!(alg, CompressionAlgorithm::Lz4);
    }

    #[test]
    fn web_asset_picks_brotli() {
        let (alg, level) = pick_auto(Some("text/html"), Some(10_000));
        assert_eq!(alg, CompressionAlgorithm::Brotli);
        assert_eq!(level, 4);

        let (alg, _) = pick_auto(Some("application/javascript"), Some(10_000));
        assert_eq!(alg, CompressionAlgorithm::Brotli);

        let (alg, _) = pick_auto(Some("image/svg+xml"), Some(10_000));
        assert_eq!(alg, CompressionAlgorithm::Brotli);
    }

    #[test]
    fn structured_text_picks_zstd() {
        let (alg, level) = pick_auto(Some("application/json"), Some(10_000));
        assert_eq!(alg, CompressionAlgorithm::Zstd);
        assert_eq!(level, 3);

        let (alg, _) = pick_auto(Some("text/plain"), Some(10_000));
        assert_eq!(alg, CompressionAlgorithm::Zstd);
    }

    #[test]
    fn unknown_picks_zstd_fallback() {
        let (alg, level) = pick_auto(Some("application/octet-stream"), Some(10_000));
        assert_eq!(alg, CompressionAlgorithm::Zstd);
        assert_eq!(level, 3);
    }

    #[test]
    fn unknown_size_picks_zstd_fallback() {
        let (alg, _) = pick_auto(None, None);
        assert_eq!(alg, CompressionAlgorithm::Zstd);
    }

    #[test]
    fn content_type_with_params_normalized() {
        let (alg, _) = pick_auto(Some("TEXT/HTML; charset=utf-8"), Some(10_000));
        assert_eq!(alg, CompressionAlgorithm::Brotli);
    }
}
