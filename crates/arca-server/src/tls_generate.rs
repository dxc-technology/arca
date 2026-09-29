//! TLS certificate generation using `rcgen`.
//!
//! Generates a self-signed CA and a server certificate signed by it.

use std::net::IpAddr;
use std::path::Path;

use anyhow::{Context, Result};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, IsCa, KeyPair, PublicKeyData, SanType,
};
use time::OffsetDateTime;

/// Certificates are public material.
const MODE_CERT: u32 = 0o644;
/// Server and node private keys. Group-readable on purpose: Arca runs as
/// 65532:65532 and the web console runs as a *different* user (uid 100), so the
/// group is the channel through which the console reaches the same key — via
/// `group_add` in compose, `fsGroup`/`supplementalGroups` in Kubernetes. Never
/// other-readable.
const MODE_KEY: u32 = 0o640;
/// The CA private key only signs; nothing reads it at runtime.
const MODE_CA_KEY: u32 = 0o600;

/// Write `contents` to `path` with an explicit Unix mode.
///
/// The mode is set at creation time so a private key is never even briefly
/// world-readable, and re-applied afterwards for two reasons: the creation mode
/// is masked by the process umask (which must not be allowed to strip the group
/// bit a key needs), and it does not apply at all when the file already exists
/// from an earlier run.
fn write_file_with_mode(path: &Path, contents: &str, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("setting permissions on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::write(path, contents)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Parameters of a self-signed CA certificate.
fn ca_params(common_name: &str, not_after: OffsetDateTime) -> Result<CertificateParams> {
    let mut params = CertificateParams::new(Vec::<String>::new()).context("creating CA params")?;
    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, common_name);
    dn.push(rcgen::DnType::OrganizationName, "Arca");
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    // OpenSSL strict verification (the default from Python 3.13) refuses a CA
    // certificate without the KeyUsage extension asserting keyCertSign.
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    params.not_after = not_after;
    Ok(params)
}

/// Parameters of a CA-signed server certificate for `sans`.
fn server_params(sans: &str, not_after: OffsetDateTime) -> Result<CertificateParams> {
    let mut params =
        CertificateParams::new(Vec::<String>::new()).context("creating server params")?;
    let mut dn = DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "Arca Server");
    dn.push(rcgen::DnType::OrganizationName, "Arca");
    params.distinguished_name = dn;
    params.subject_alt_names = parse_sans(sans);
    // Modern verifiers (OpenSSL 3.x strict mode, as shipped by current
    // Python/curl) refuse CA-issued certs without an Authority Key
    // Identifier; rcgen does not emit it by default.
    params.use_authority_key_identifier_extension = true;
    params.not_after = not_after;
    Ok(params)
}

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

    let now = OffsetDateTime::now_utc();

    // --- CA ---
    let ca_key = KeyPair::generate().context("generating CA key pair")?;
    let ca_params = ca_params("Arca CA", now + time::Duration::days(i64::from(days) * 2))?;
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .context("self-signing CA certificate")?;

    // --- Server cert ---
    let server_key = KeyPair::generate().context("generating server key pair")?;
    let server_params = server_params(sans, now + time::Duration::days(i64::from(days)))?;
    let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let server_cert = server_params
        .signed_by(&server_key, &ca_issuer)
        .context("signing server certificate")?;

    // --- Write files ---
    let ca_cert_path = output_dir.join("arca-ca.crt");
    let ca_key_path = output_dir.join("arca-ca.key");
    let server_cert_path = output_dir.join("arca-server.crt");
    let server_key_path = output_dir.join("arca-server.key");

    write_file_with_mode(&ca_cert_path, &ca_cert.pem(), MODE_CERT)?;
    write_file_with_mode(&ca_key_path, &ca_key.serialize_pem(), MODE_CA_KEY)?;
    write_file_with_mode(&server_cert_path, &server_cert.pem(), MODE_CERT)?;
    write_file_with_mode(&server_key_path, &server_key.serialize_pem(), MODE_KEY)?;

    println!("TLS certificates generated:");
    println!("  CA cert:     {}", ca_cert_path.display());
    println!("  CA key:      {}", ca_key_path.display());
    println!("  Server cert: {}", server_cert_path.display());
    println!("  Server key:  {}", server_key_path.display());
    println!();
    println!("Private keys are not world-readable: server key 0640, CA key 0600.");
    println!("A process that reads the server key under a different user (the web");
    println!("console does) must belong to the key's group — see the TLS guide.");
    println!();
    println!("Add to your config.toml:");
    println!();
    println!("  [server.tls]");
    println!("  cert_dir = \"{}\"", output_dir.display());
    println!("  cert_file = \"arca-server.crt\"");
    println!("  key_file = \"arca-server.key\"");

    Ok(())
}

/// File names of the local TLS material written by [`ensure`].
pub const SERVER_CERT: &str = "arca-server.crt";
pub const SERVER_KEY: &str = "arca-server.key";
pub const CA_CERT: &str = "arca-ca.crt";
pub const CA_KEY: &str = "arca-ca.key";

/// Validity and renewal policy for [`ensure`].
#[derive(Debug, Clone, Copy)]
pub struct EnsurePolicy {
    pub server_days: u32,
    pub ca_days: u32,
    pub renew_within_days: u32,
}

/// Why [`ensure`] (re)generated something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Missing,
    Unreadable,
    Expiring,
    SansChanged,
    NewCa,
    NotSignedByCa,
}

/// What [`ensure`] did to one artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Kept,
    Generated(Reason),
}

/// What [`ensure`] did, for the CA and for the server certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsureOutcome {
    pub ca: Action,
    pub server: Action,
}

/// Make sure `output_dir` holds a server certificate valid for `sans`, signed
/// by the local CA kept in `ca_dir`, (re)generating only what is missing,
/// unreadable, expiring within the renewal window, or no longer matching.
pub fn ensure(
    output_dir: &Path,
    ca_dir: &Path,
    sans: &str,
    policy: EnsurePolicy,
    now: OffsetDateTime,
) -> Result<EnsureOutcome> {
    for dir in [output_dir, ca_dir] {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating directory: {}", dir.display()))?;
    }
    let window = time::Duration::days(i64::from(policy.renew_within_days));
    let wanted_sans = normalize_sans(sans);

    let reuse = match load_local_ca(ca_dir) {
        Ok(Some(ca)) if ca.not_after - now > window => Ok(ca),
        Ok(Some(_)) => Err(Reason::Expiring),
        Ok(None) => Err(Reason::Missing),
        Err(_) => Err(Reason::Unreadable),
    };
    let (ca_action, ca) = match reuse {
        Ok(ca) => (Action::Kept, ca),
        Err(reason) => (
            Action::Generated(reason),
            new_local_ca(ca_dir, now + time::Duration::days(i64::from(policy.ca_days)))?,
        ),
    };

    let server_exists =
        output_dir.join(SERVER_CERT).exists() && output_dir.join(SERVER_KEY).exists();
    let server_action = if ca_action != Action::Kept {
        Action::Generated(if server_exists { Reason::NewCa } else { Reason::Missing })
    } else {
        match check_server(output_dir, &ca, &wanted_sans, now, window) {
            None => Action::Kept,
            Some(reason) => Action::Generated(reason),
        }
    };
    if server_action != Action::Kept {
        issue_server(
            output_dir,
            &ca,
            &wanted_sans.join(","),
            now + time::Duration::days(i64::from(policy.server_days)),
        )?;
    }

    // Publish the CA certificate next to the server material, for clients to
    // trust; the CA key stays in `ca_dir`.
    let published = output_dir.join(CA_CERT);
    if std::fs::read_to_string(&published).ok().as_deref() != Some(ca.pem.as_str()) {
        write_file_with_mode(&published, &ca.pem, MODE_CERT)?;
    }

    Ok(EnsureOutcome {
        ca: ca_action,
        server: server_action,
    })
}

/// A human-readable summary of what [`ensure`] did.
pub fn describe(outcome: &EnsureOutcome, output_dir: &Path, ca_dir: &Path) -> String {
    fn why(reason: &Reason) -> &'static str {
        match reason {
            Reason::Missing => "missing",
            Reason::Unreadable => "unreadable or inconsistent",
            Reason::Expiring => "expiring",
            Reason::SansChanged => "SANs changed",
            Reason::NewCa => "new CA",
            Reason::NotSignedByCa => "not signed by the local CA",
        }
    }
    let line = |what: &str, action: &Action| match action {
        Action::Kept => format!("{what}: up to date"),
        Action::Generated(reason) => format!("{what}: generated ({})", why(reason)),
    };
    let mut out = format!(
        "{}\n{}",
        line(&format!("Local CA ({})", ca_dir.join(CA_CERT).display()), &outcome.ca),
        line(
            &format!("Server certificate ({})", output_dir.join(SERVER_CERT).display()),
            &outcome.server
        ),
    );
    if outcome.ca != Action::Kept {
        out.push_str(&format!(
            "\nNew local CA: trust {} once in your OS/browser to avoid certificate warnings.",
            ca_dir.join(CA_CERT).display()
        ));
    }
    out
}

/// The local CA read back from `ca_dir`.
struct LocalCa {
    pem: String,
    key: KeyPair,
    not_after: OffsetDateTime,
}

/// Reads the local CA; `Ok(None)` when it does not exist yet, an error when
/// it exists but cannot be used (unparseable, or a key that does not match).
fn load_local_ca(ca_dir: &Path) -> Result<Option<LocalCa>> {
    let (cert_path, key_path) = (ca_dir.join(CA_CERT), ca_dir.join(CA_KEY));
    if !cert_path.exists() || !key_path.exists() {
        return Ok(None);
    }
    let pem = std::fs::read_to_string(&cert_path)?;
    let key = KeyPair::from_pem(&std::fs::read_to_string(&key_path)?)?;
    let (_, parsed) = x509_parser::pem::parse_x509_pem(pem.as_bytes())?;
    let cert = parsed.parse_x509()?;
    if cert.public_key().raw != key.subject_public_key_info().as_slice() {
        anyhow::bail!("the CA key does not match the CA certificate");
    }
    Ok(Some(LocalCa {
        not_after: cert.validity().not_after.to_datetime(),
        pem,
        key,
    }))
}

fn new_local_ca(ca_dir: &Path, not_after: OffsetDateTime) -> Result<LocalCa> {
    let key = KeyPair::generate().context("generating local CA key pair")?;
    let cert = ca_params("Arca Local CA", not_after)?
        .self_signed(&key)
        .context("self-signing local CA certificate")?;
    let pem = cert.pem();
    write_file_with_mode(&ca_dir.join(CA_CERT), &pem, MODE_CERT)?;
    write_file_with_mode(&ca_dir.join(CA_KEY), &key.serialize_pem(), MODE_CA_KEY)?;
    Ok(LocalCa { pem, key, not_after })
}

/// Why the server certificate in `output_dir` must be reissued, if it must.
fn check_server(
    output_dir: &Path,
    ca: &LocalCa,
    wanted_sans: &[String],
    now: OffsetDateTime,
    window: time::Duration,
) -> Option<Reason> {
    let (cert_path, key_path) = (output_dir.join(SERVER_CERT), output_dir.join(SERVER_KEY));
    if !cert_path.exists() || !key_path.exists() {
        return Some(Reason::Missing);
    }
    let usable = (|| -> Result<Option<Reason>> {
        let pem = std::fs::read(&cert_path)?;
        let key = KeyPair::from_pem(&std::fs::read_to_string(&key_path)?)?;
        let (_, parsed) = x509_parser::pem::parse_x509_pem(&pem)?;
        let cert = parsed.parse_x509()?;
        if cert.public_key().raw != key.subject_public_key_info().as_slice() {
            anyhow::bail!("the server key does not match the server certificate");
        }
        let (_, ca_parsed) = x509_parser::pem::parse_x509_pem(ca.pem.as_bytes())?;
        let ca_cert = ca_parsed.parse_x509()?;
        if cert.verify_signature(Some(ca_cert.public_key())).is_err() {
            return Ok(Some(Reason::NotSignedByCa));
        }
        if cert.validity().not_after.to_datetime() - now <= window {
            return Ok(Some(Reason::Expiring));
        }
        let mut have = cert_sans(&cert);
        have.sort();
        have.dedup();
        if have != wanted_sans {
            return Ok(Some(Reason::SansChanged));
        }
        Ok(None)
    })();
    usable.unwrap_or(Some(Reason::Unreadable))
}

fn issue_server(
    output_dir: &Path,
    ca: &LocalCa,
    sans: &str,
    not_after: OffsetDateTime,
) -> Result<()> {
    let key = KeyPair::generate().context("generating server key pair")?;
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca.pem, &ca.key)
        .context("loading the local CA as issuer")?;
    let cert = server_params(sans, not_after)?
        .signed_by(&key, &issuer)
        .context("signing server certificate")?;
    write_file_with_mode(&output_dir.join(SERVER_CERT), &cert.pem(), MODE_CERT)?;
    write_file_with_mode(&output_dir.join(SERVER_KEY), &key.serialize_pem(), MODE_KEY)?;
    Ok(())
}

/// SANs as compared by [`ensure`]: DNS names lowercased, IPs in canonical
/// form, sorted and deduplicated.
fn normalize_sans(sans: &str) -> Vec<String> {
    let mut out: Vec<String> = sans
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.parse::<IpAddr>() {
            Ok(ip) => ip.to_string(),
            Err(_) => s.to_ascii_lowercase(),
        })
        .collect();
    out.sort();
    out.dedup();
    out
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
    write_file_with_mode(&ca_cert_path, &ca_cert.pem(), MODE_CERT)?;
    write_file_with_mode(&ca_key_path, &ca_key.serialize_pem(), MODE_CA_KEY)?;

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
        write_file_with_mode(&cert_path, &cert.pem(), MODE_CERT)?;
        write_file_with_mode(&key_path, &key.serialize_pem(), MODE_KEY)?;
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
/// DNS and IP SANs of a certificate, normalized like [`normalize_sans`].
fn cert_sans(cert: &x509_parser::certificate::X509Certificate<'_>) -> Vec<String> {
    use x509_parser::extensions::GeneralName;
    let Ok(Some(ext)) = cert.subject_alternative_name() else {
        return Vec::new();
    };
    ext.value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::DNSName(dns) => Some(dns.to_ascii_lowercase()),
            GeneralName::IPAddress(bytes) => match bytes.len() {
                4 => Some(IpAddr::from(<[u8; 4]>::try_from(*bytes).ok()?).to_string()),
                16 => Some(IpAddr::from(<[u8; 16]>::try_from(*bytes).ok()?).to_string()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

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

        // Node keys stay group-readable (the console reads them through Arca's
        // service GID), the CA key is owner-only, certificates are public.
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&dir.path().join("arca-cluster-ca.crt")), 0o644);
            assert_eq!(mode_of(&dir.path().join("arca-cluster-ca.key")), 0o600);
            for node in ["arca-1", "arca-2"] {
                assert_eq!(mode_of(&dir.path().join(format!("{node}.crt"))), 0o644);
                assert_eq!(mode_of(&dir.path().join(format!("{node}.key"))), 0o640);
            }
        }
    }

    // --- ensure -----------------------------------------------------------

    const POLICY: EnsurePolicy = EnsurePolicy {
        server_days: 365,
        ca_days: 3650,
        renew_within_days: 30,
    };
    const SANS: &str = "localhost,127.0.0.1,::1,arca";

    fn now() -> time::OffsetDateTime {
        time::OffsetDateTime::now_utc()
    }

    /// A pair of scratch directories: (output_dir, ca_dir).
    fn dirs() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("local");
        let ca = root.path().join("local-ca");
        (root, out, ca)
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }

    fn parse_pem_cert(path: &Path) -> (Vec<u8>, x509_parser::pem::Pem) {
        let data = read(path);
        let (_, pem) = x509_parser::pem::parse_x509_pem(&data).unwrap();
        (data, pem)
    }

    /// SANs of a certificate, as sorted strings.
    fn sans_of(path: &Path) -> Vec<String> {
        let (_, pem) = parse_pem_cert(path);
        let cert = pem.parse_x509().unwrap();
        let mut sans = cert_sans(&cert);
        sans.sort();
        sans
    }

    fn signed_by(cert_path: &Path, ca_path: &Path) -> bool {
        let (_, cert_pem) = parse_pem_cert(cert_path);
        let (_, ca_pem) = parse_pem_cert(ca_path);
        let cert = cert_pem.parse_x509().unwrap();
        let ca = ca_pem.parse_x509().unwrap();
        cert.verify_signature(Some(ca.public_key())).is_ok()
    }

    fn validity_days(path: &Path) -> i64 {
        let (_, pem) = parse_pem_cert(path);
        let cert = pem.parse_x509().unwrap();
        (cert.validity().not_after.to_datetime() - now()).whole_days()
    }

    #[test]
    fn ensure_creates_ca_and_server_in_fresh_dirs() {
        let (_root, out, ca) = dirs();
        let outcome = ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(outcome.ca, Action::Generated(Reason::Missing));
        assert_eq!(outcome.server, Action::Generated(Reason::Missing));

        for f in [SERVER_CERT, SERVER_KEY, CA_CERT] {
            assert!(out.join(f).is_file(), "missing {f} in output dir");
        }
        for f in [CA_CERT, CA_KEY] {
            assert!(ca.join(f).is_file(), "missing {f} in CA dir");
        }
        // The CA key never reaches the directory mounted in containers.
        assert!(!out.join(CA_KEY).exists());
        // The output dir publishes the CA certificate clients must trust.
        assert_eq!(read(&out.join(CA_CERT)), read(&ca.join(CA_CERT)));
        assert!(signed_by(&out.join(SERVER_CERT), &ca.join(CA_CERT)));
        assert_eq!(sans_of(&out.join(SERVER_CERT)), vec!["127.0.0.1", "::1", "arca", "localhost"]);
    }

    #[test]
    #[cfg(unix)]
    fn ensure_writes_the_same_modes_as_generate() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(mode_of(&out.join(SERVER_CERT)), 0o644);
        assert_eq!(mode_of(&out.join(SERVER_KEY)), 0o640);
        assert_eq!(mode_of(&out.join(CA_CERT)), 0o644);
        assert_eq!(mode_of(&ca.join(CA_CERT)), 0o644);
        assert_eq!(mode_of(&ca.join(CA_KEY)), 0o600);
    }

    #[test]
    fn ensure_uses_ten_years_for_the_ca_and_one_year_for_the_server() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert!((3649..=3650).contains(&validity_days(&ca.join(CA_CERT))));
        assert!((364..=365).contains(&validity_days(&out.join(SERVER_CERT))));
    }

    #[test]
    fn ensure_is_idempotent() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let before: Vec<_> = [out.join(SERVER_CERT), out.join(SERVER_KEY), ca.join(CA_KEY)]
            .iter()
            .map(|p| read(p))
            .collect();

        let outcome = ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(outcome, EnsureOutcome { ca: Action::Kept, server: Action::Kept });
        let after: Vec<_> = [out.join(SERVER_CERT), out.join(SERVER_KEY), ca.join(CA_KEY)]
            .iter()
            .map(|p| read(p))
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn ensure_treats_san_order_and_case_as_irrelevant() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let outcome = ensure(&out, &ca, "ARCA, ::1,127.0.0.1 ,localhost", POLICY, now()).unwrap();
        assert_eq!(outcome.server, Action::Kept);
    }

    #[test]
    fn ensure_renews_an_expiring_server_certificate_with_the_same_ca() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let ca_cert = read(&ca.join(CA_CERT));
        let ca_key = read(&ca.join(CA_KEY));
        let old_server = read(&out.join(SERVER_CERT));

        // 340 days later the 365-day certificate is inside the 30-day window.
        let later = now() + time::Duration::days(340);
        let outcome = ensure(&out, &ca, SANS, POLICY, later).unwrap();
        assert_eq!(outcome.ca, Action::Kept);
        assert_eq!(outcome.server, Action::Generated(Reason::Expiring));

        // Same CA, byte for byte: whoever trusted it keeps trusting.
        assert_eq!(read(&ca.join(CA_CERT)), ca_cert);
        assert_eq!(read(&ca.join(CA_KEY)), ca_key);
        assert_ne!(read(&out.join(SERVER_CERT)), old_server);
        assert!(signed_by(&out.join(SERVER_CERT), &ca.join(CA_CERT)));
    }

    #[test]
    fn ensure_keeps_a_server_certificate_outside_the_renewal_window() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let later = now() + time::Duration::days(300);
        let outcome = ensure(&out, &ca, SANS, POLICY, later).unwrap();
        assert_eq!(outcome, EnsureOutcome { ca: Action::Kept, server: Action::Kept });
    }

    #[test]
    fn ensure_renews_the_server_only_when_sans_change() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let ca_cert = read(&ca.join(CA_CERT));

        let sans = format!("{SANS},s3.example.org");
        let outcome = ensure(&out, &ca, &sans, POLICY, now()).unwrap();
        assert_eq!(outcome.ca, Action::Kept);
        assert_eq!(outcome.server, Action::Generated(Reason::SansChanged));
        assert_eq!(read(&ca.join(CA_CERT)), ca_cert);
        assert!(sans_of(&out.join(SERVER_CERT)).contains(&"s3.example.org".to_string()));
    }

    #[test]
    fn ensure_renews_an_expiring_ca_and_reissues_the_server() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let old_ca = read(&ca.join(CA_CERT));

        let later = now() + time::Duration::days(3640);
        let outcome = ensure(&out, &ca, SANS, POLICY, later).unwrap();
        assert_eq!(outcome.ca, Action::Generated(Reason::Expiring));
        assert_eq!(outcome.server, Action::Generated(Reason::NewCa));
        assert_ne!(read(&ca.join(CA_CERT)), old_ca);
        assert_eq!(read(&out.join(CA_CERT)), read(&ca.join(CA_CERT)));
        assert!(signed_by(&out.join(SERVER_CERT), &ca.join(CA_CERT)));
    }

    #[test]
    fn ensure_reissues_a_server_certificate_not_signed_by_the_current_ca() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        // The CA directory is swapped for another CA by hand.
        let (_other_root, other_out, other_ca) = dirs();
        ensure(&other_out, &other_ca, SANS, POLICY, now()).unwrap();
        for f in [CA_CERT, CA_KEY] {
            std::fs::copy(other_ca.join(f), ca.join(f)).unwrap();
        }

        let outcome = ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(outcome.ca, Action::Kept);
        assert_eq!(outcome.server, Action::Generated(Reason::NotSignedByCa));
        assert!(signed_by(&out.join(SERVER_CERT), &ca.join(CA_CERT)));
        assert_eq!(read(&out.join(CA_CERT)), read(&ca.join(CA_CERT)));
    }

    #[test]
    fn ensure_reissues_the_server_when_its_key_is_missing() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        std::fs::remove_file(out.join(SERVER_KEY)).unwrap();
        let outcome = ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(outcome.ca, Action::Kept);
        assert_eq!(outcome.server, Action::Generated(Reason::Missing));
        assert!(out.join(SERVER_KEY).is_file());
    }

    #[test]
    fn ensure_republishes_a_missing_ca_certificate_without_touching_the_rest() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let server = read(&out.join(SERVER_CERT));
        std::fs::remove_file(out.join(CA_CERT)).unwrap();

        let outcome = ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(outcome, EnsureOutcome { ca: Action::Kept, server: Action::Kept });
        assert_eq!(read(&out.join(CA_CERT)), read(&ca.join(CA_CERT)));
        assert_eq!(read(&out.join(SERVER_CERT)), server);
    }

    #[test]
    fn ensure_replaces_an_unreadable_ca() {
        let (_root, out, ca) = dirs();
        std::fs::create_dir_all(&ca).unwrap();
        std::fs::write(ca.join(CA_CERT), "garbage").unwrap();
        std::fs::write(ca.join(CA_KEY), "garbage").unwrap();
        let outcome = ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(outcome.ca, Action::Generated(Reason::Unreadable));
        assert!(signed_by(&out.join(SERVER_CERT), &ca.join(CA_CERT)));
    }

    #[test]
    fn ensure_replaces_a_ca_whose_key_does_not_match_its_certificate() {
        let (_root, out, ca) = dirs();
        ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        let (_other_root, other_out, other_ca) = dirs();
        ensure(&other_out, &other_ca, SANS, POLICY, now()).unwrap();
        std::fs::copy(other_ca.join(CA_KEY), ca.join(CA_KEY)).unwrap();

        let outcome = ensure(&out, &ca, SANS, POLICY, now()).unwrap();
        assert_eq!(outcome.ca, Action::Generated(Reason::Unreadable));
        assert!(signed_by(&out.join(SERVER_CERT), &ca.join(CA_CERT)));
    }

    /// The permission bits of a path, as `0o644`-style Unix mode.
    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    #[cfg(unix)]
    fn generate_never_writes_world_readable_private_keys() {
        let dir = tempfile::tempdir().unwrap();
        generate(dir.path(), "localhost,127.0.0.1", 365).unwrap();

        // Certificates are public material.
        assert_eq!(mode_of(&dir.path().join("arca-ca.crt")), 0o644);
        assert_eq!(mode_of(&dir.path().join("arca-server.crt")), 0o644);
        // The server key is the one the console also reads, through group
        // membership — group-readable, never other-readable.
        assert_eq!(mode_of(&dir.path().join("arca-server.key")), 0o640);
        // Nothing reads the CA key at runtime; it only signs.
        assert_eq!(mode_of(&dir.path().join("arca-ca.key")), 0o600);
    }

    #[test]
    #[cfg(unix)]
    fn generate_tightens_the_mode_of_pre_existing_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        // A previous run of an older Arca left the keys world-readable.
        for file in ["arca-ca.key", "arca-server.key"] {
            let path = dir.path().join(file);
            std::fs::write(&path, "stale").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        }

        generate(dir.path(), "localhost", 365).unwrap();

        assert_eq!(mode_of(&dir.path().join("arca-server.key")), 0o640);
        assert_eq!(mode_of(&dir.path().join("arca-ca.key")), 0o600);
    }
}
