//! Virtual-hosted-style addressing: `bucket.example.com/key` → `/bucket/key`.
//!
//! The pure rewrite rules, applied by
//! [`NormalizeLayer`](super::normalize::NormalizeLayer) before routing when a
//! server domain is configured. They cannot be a `Router::layer` middleware:
//! that runs after routing, when the path parameters are already bound to the
//! unrewritten path (`bucket.example.com/key` would be routed as bucket `key`).

/// The bucket a `Host` header addresses: `{bucket}.{domain}` (port ignored).
/// None for any other host, including `domain` itself (path-style) and a
/// multi-label prefix such as `a.b.{domain}`.
pub fn bucket_for_host<'a>(host: &'a str, domain: &str) -> Option<&'a str> {
    let host = host.split(':').next().unwrap_or(host);
    let bucket = host.strip_suffix(domain)?.strip_suffix('.')?;
    (!bucket.is_empty() && !bucket.contains('.')).then_some(bucket)
}

/// The path-style path of a virtual-hosted request for `bucket`: the bucket
/// root (`/`) becomes `/{bucket}`, any key path gets `/{bucket}` prepended.
pub fn path_style_path(bucket: &str, path: &str) -> String {
    if path == "/" {
        format!("/{bucket}")
    } else {
        format!("/{bucket}{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_a_bucket_under_the_domain() {
        assert_eq!(bucket_for_host("photos.s3.example.com", "s3.example.com"), Some("photos"));
    }

    #[test]
    fn the_port_is_ignored() {
        assert_eq!(bucket_for_host("photos.s3.example.com:9000", "s3.example.com"), Some("photos"));
    }

    #[test]
    fn the_domain_itself_is_path_style() {
        assert_eq!(bucket_for_host("s3.example.com", "s3.example.com"), None);
        assert_eq!(bucket_for_host("s3.example.com:9000", "s3.example.com"), None);
    }

    #[test]
    fn other_hosts_are_path_style() {
        assert_eq!(bucket_for_host("localhost:9000", "s3.example.com"), None);
        assert_eq!(bucket_for_host("photos.other.com", "s3.example.com"), None);
        // A suffix match must be on a label boundary.
        assert_eq!(bucket_for_host("evils3.example.com", "s3.example.com"), None);
    }

    #[test]
    fn a_dotted_prefix_is_not_a_bucket() {
        assert_eq!(bucket_for_host("a.b.s3.example.com", "s3.example.com"), None);
        assert_eq!(bucket_for_host(".s3.example.com", "s3.example.com"), None);
    }

    #[test]
    fn the_bucket_root_maps_to_the_bucket_path() {
        assert_eq!(path_style_path("b", "/"), "/b");
    }

    #[test]
    fn a_key_path_gets_the_bucket_prepended() {
        assert_eq!(path_style_path("b", "/k"), "/b/k");
        assert_eq!(path_style_path("b", "/a/b/"), "/b/a/b/");
    }
}
