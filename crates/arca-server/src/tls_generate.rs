//! TLS certificate generation using `rcgen`.
//!
//! Generates a self-signed CA and a server certificate signed by it.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, IsCa, KeyPair, SanType};

/// Generate a self-signed CA + server certificate pair.
///
/// Writes four files to `output_dir`:
/// - `arca-ca.crt` — CA certificate
/// - `arca-ca.key` — CA private key
/// - `arca-server.crt` — server certificate (signed by CA)
/// - `arca-server.key` — server private key
pub fn generate(output_dir: &Path, sans: &str, days: u32) -> Result<()> {
    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("creating output dir: {}", output_dir.display()))?;

    // --- CA ---
    let ca_key = KeyPair::generate().context("generating CA key pair")?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())
        .context("creating CA params")?;
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(rcgen::DnType::CommonName, "Arca CA");
    ca_dn.push(rcgen::DnType::OrganizationName, "Arca");
    ca_params.distinguished_name = ca_dn;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.not_after = time::OffsetDateTime::now_utc()
        + time::Duration::days(i64::from(days) * 2);
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("self-signing CA certificate")?;

    // --- Server cert ---
    let server_key = KeyPair::generate().context("generating server key pair")?;
    let san_types = parse_sans(sans);
    let mut server_params = CertificateParams::new(Vec::<String>::new())
        .context("creating server params")?;
    let mut server_dn = DistinguishedName::new();
    server_dn.push(rcgen::DnType::CommonName, "Arca Server");
    server_dn.push(rcgen::DnType::OrganizationName, "Arca");
    server_params.distinguished_name = server_dn;
    server_params.subject_alt_names = san_types;
    server_params.not_after = time::OffsetDateTime::now_utc()
        + time::Duration::days(i64::from(days));
    let server_cert = server_params
        .signed_by(&server_key, &ca_cert, &ca_key)
        .context("signing server certificate")?;

    // --- Write files ---
    let ca_cert_path = output_dir.join("arca-ca.crt");
    let ca_key_path = output_dir.join("arca-ca.key");
    let server_cert_path = output_dir.join("arca-server.crt");
    let server_key_path = output_dir.join("arca-server.key");

    std::fs::write(&ca_cert_path, ca_cert.pem())
        .with_context(|| format!("writing {}", ca_cert_path.display()))?;
    std::fs::write(&ca_key_path, ca_key.serialize_pem())
        .with_context(|| format!("writing {}", ca_key_path.display()))?;
    std::fs::write(&server_cert_path, server_cert.pem())
        .with_context(|| format!("writing {}", server_cert_path.display()))?;
    std::fs::write(&server_key_path, server_key.serialize_pem())
        .with_context(|| format!("writing {}", server_key_path.display()))?;

    println!("TLS certificates generated:");
    println!("  CA cert:     {}", ca_cert_path.display());
    println!("  CA key:      {}", ca_key_path.display());
    println!("  Server cert: {}", server_cert_path.display());
    println!("  Server key:  {}", server_key_path.display());
    println!();
    println!("Add to your config.toml:");
    println!();
    println!("  [server.tls]");
    println!("  cert_dir = \"{}\"", output_dir.display());
    println!("  cert_file = \"arca-server.crt\"");
    println!("  key_file = \"arca-server.key\"");

    Ok(())
}

/// Parse a comma-separated SANs string into `SanType` values.
/// Distinguishes IP addresses from DNS names.
fn parse_sans(sans: &str) -> Vec<SanType> {
    sans.split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            if let Ok(ip) = s.parse::<IpAddr>() {
                SanType::IpAddress(ip)
            } else {
                SanType::DnsName(s.to_string().try_into().expect("valid DNS name"))
            }
        })
        .collect()
}
