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
    // OpenSSL strict verification (the default from Python 3.13) refuses a CA
    // certificate without the KeyUsage extension asserting keyCertSign.
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
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
    // Modern verifiers (OpenSSL 3.x strict mode, as shipped by current
    // Python/curl) refuse CA-issued certs without an Authority Key
    // Identifier; rcgen does not emit it by default.
    server_params.use_authority_key_identifier_extension = true;
    server_params.not_after = time::OffsetDateTime::now_utc()
        + time::Duration::days(i64::from(days));
    let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let server_cert = server_params
        .signed_by(&server_key, &ca_issuer)
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

/// Generate a cluster CA + one CA-signed certificate per node, for inter-node
/// mutual TLS (`[cluster.tls]`, decision H12 — resolves TD-015).
///
/// Each `nodes` entry is `name` or `name=san1,san2,...`; the SANs must cover
/// every DNS name/IP peers use to reach that node (they default to the name).
/// Writes to `output_dir`:
/// - `arca-cluster-ca.crt` / `arca-cluster-ca.key` — the CA (same `ca_file` on
///   every node; keep the key offline, it is only needed to mint more certs)
/// - `<name>.crt` / `<name>.key` — per node, signed by the CA, with both
///   serverAuth and clientAuth EKUs: the same pair serves as the node's
///   listener certificate and as its inter-node client identity.
pub fn generate_cluster(output_dir: &Path, nodes: &[String], days: u32) -> Result<()> {
    let specs = nodes
        .iter()
        .map(|s| parse_node_spec(s))
        .collect::<Result<Vec<_>>>()?;

    std::fs::create_dir_all(output_dir)
        .with_context(|| format!("creating output dir: {}", output_dir.display()))?;

    // --- Cluster CA (DN distinct from the node certs — rcgen gotcha) ---
    let ca_key = KeyPair::generate().context("generating cluster CA key pair")?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())
        .context("creating cluster CA params")?;
    let mut ca_dn = DistinguishedName::new();
    ca_dn.push(rcgen::DnType::CommonName, "Arca Cluster CA");
    ca_dn.push(rcgen::DnType::OrganizationName, "Arca");
    ca_params.distinguished_name = ca_dn;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    // OpenSSL strict verification (the default from Python 3.13) refuses a CA
    // certificate without the KeyUsage extension asserting keyCertSign.
    ca_params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    ca_params.not_after =
        time::OffsetDateTime::now_utc() + time::Duration::days(i64::from(days) * 2);
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("self-signing cluster CA certificate")?;

    let ca_cert_path = output_dir.join("arca-cluster-ca.crt");
    let ca_key_path = output_dir.join("arca-cluster-ca.key");
    std::fs::write(&ca_cert_path, ca_cert.pem())
        .with_context(|| format!("writing {}", ca_cert_path.display()))?;
    std::fs::write(&ca_key_path, ca_key.serialize_pem())
        .with_context(|| format!("writing {}", ca_key_path.display()))?;

    // --- Per-node certificates ---
    let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    for (name, sans) in &specs {
        let key = KeyPair::generate()
            .with_context(|| format!("generating key pair for node {name}"))?;
        let mut params = CertificateParams::new(Vec::<String>::new())
            .with_context(|| format!("creating params for node {name}"))?;
        let mut dn = DistinguishedName::new();
        dn.push(rcgen::DnType::CommonName, name.as_str());
        dn.push(rcgen::DnType::OrganizationName, "Arca Cluster");
        params.distinguished_name = dn;
        params.subject_alt_names = sans.clone();
        // The same cert is presented BY the listener (serverAuth) and AS the
        // inter-node client identity (clientAuth).
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        // Same AKI rationale as the server certificate above.
        params.use_authority_key_identifier_extension = true;
        params.not_after =
            time::OffsetDateTime::now_utc() + time::Duration::days(i64::from(days));
        let cert = params
            .signed_by(&key, &ca_issuer)
            .with_context(|| format!("signing certificate for node {name}"))?;

        let cert_path = output_dir.join(format!("{name}.crt"));
        let key_path = output_dir.join(format!("{name}.key"));
        std::fs::write(&cert_path, cert.pem())
            .with_context(|| format!("writing {}", cert_path.display()))?;
        std::fs::write(&key_path, key.serialize_pem())
            .with_context(|| format!("writing {}", key_path.display()))?;
    }

    println!("Cluster TLS material generated in {}:", output_dir.display());
    println!("  CA cert: arca-cluster-ca.crt  (same ca_file on EVERY node)");
    println!("  CA key:  arca-cluster-ca.key  (keep offline — only needed to mint more node certs)");
    for (name, _) in &specs {
        println!("  node {name}: {name}.crt / {name}.key");
    }
    println!();
    println!("On each node, point both sections at ITS OWN cert/key:");
    println!();
    println!("  [server.tls]");
    println!("  cert_file = \"{}/<node>.crt\"", output_dir.display());
    println!("  key_file = \"{}/<node>.key\"", output_dir.display());
    println!();
    println!("  [cluster.tls]");
    println!("  ca_file = \"{}\"", ca_cert_path.display());
    println!("  cert_file = \"{}/<node>.crt\"", output_dir.display());
    println!("  key_file = \"{}/<node>.key\"", output_dir.display());
    println!();
    println!("S3 clients must trust the CA too (e.g. `aws --ca-bundle {}`),", ca_cert_path.display());
    println!("or keep a separate public certificate in [server.tls].");

    Ok(())
}

/// Parses a `--node` spec: `name` or `name=san1,san2,...`. The name becomes
/// the certificate CN and the output file names; the SANs default to the name.
fn parse_node_spec(spec: &str) -> Result<(String, Vec<SanType>)> {
    let (name, sans) = match spec.split_once('=') {
        Some((n, s)) => (n.trim(), s),
        None => (spec.trim(), ""),
    };
    if name.is_empty() {
        anyhow::bail!("invalid --node spec {spec:?}: empty node name");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        anyhow::bail!(
            "invalid --node spec {spec:?}: the name becomes a file name, \
             use only letters, digits, '-', '_', '.'"
        );
    }
    let san_types = if sans.trim().is_empty() {
        parse_sans(name)
    } else {
        parse_sans(sans)
    };
    Ok((name.to_string(), san_types))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_spec_name_only_defaults_sans_to_name() {
        let (name, sans) = parse_node_spec("arca-1").unwrap();
        assert_eq!(name, "arca-1");
        assert_eq!(sans.len(), 1);
        assert!(matches!(&sans[0], SanType::DnsName(d) if d.as_str() == "arca-1"));
    }

    #[test]
    fn node_spec_with_explicit_sans() {
        let (name, sans) = parse_node_spec("arca-1=arca-1.internal,10.0.0.1").unwrap();
        assert_eq!(name, "arca-1");
        assert_eq!(sans.len(), 2);
        assert!(matches!(&sans[0], SanType::DnsName(d) if d.as_str() == "arca-1.internal"));
        assert!(matches!(&sans[1], SanType::IpAddress(ip) if ip.to_string() == "10.0.0.1"));
    }

    #[test]
    fn node_spec_rejects_empty_or_unsafe_names() {
        assert!(parse_node_spec("").is_err());
        assert!(parse_node_spec("=10.0.0.1").is_err());
        assert!(parse_node_spec("../evil").is_err());
        assert!(parse_node_spec("a b").is_err());
    }

    #[test]
    fn generate_cluster_writes_ca_and_node_files() {
        let dir = tempfile::tempdir().unwrap();
        generate_cluster(
            dir.path(),
            &["arca-1=arca-1,127.0.0.1".to_string(), "arca-2".to_string()],
            365,
        )
        .unwrap();

        for file in [
            "arca-cluster-ca.crt",
            "arca-cluster-ca.key",
            "arca-1.crt",
            "arca-1.key",
            "arca-2.crt",
            "arca-2.key",
        ] {
            assert!(dir.path().join(file).is_file(), "missing {file}");
        }

        // The node cert + CA parse as certificates, the keys as private keys.
        use rustls::pki_types::pem::PemObject;
        let cert_data = std::fs::read(dir.path().join("arca-1.crt")).unwrap();
        let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_slice_iter(&cert_data)
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(certs.len(), 1);
        let key_data = std::fs::read(dir.path().join("arca-1.key")).unwrap();
        assert!(rustls::pki_types::PrivatePkcs8KeyDer::from_pem_slice(&key_data).is_ok());
    }
}
