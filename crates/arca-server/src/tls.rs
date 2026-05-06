//! TLS support: certificate loading, auto-detection, hot-reload, and HTTPS serving.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use hyper_util::rt::TokioIo;
use rustls::ServerConfig as RustlsServerConfig;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::config::{HttpConfig, ResolvedPaths, TlsConfig};

// ---------------------------------------------------------------------------
// PEM auto-detection
// ---------------------------------------------------------------------------

/// Scan a directory for PEM files and classify them as cert or key.
/// Returns (cert_path, key_path). Fails if ambiguous.
pub fn detect_pem_files(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let mut cert_files: Vec<PathBuf> = Vec::new();
    let mut key_files: Vec<PathBuf> = Vec::new();

    let entries = std::fs::read_dir(dir)
        .with_context(|| format!("reading TLS cert_dir: {}", dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        if !matches!(ext, "pem" | "crt" | "cert" | "key") {
            continue;
        }

        let data = std::fs::read(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        let mut has_key = false;
        let mut has_cert = false;
        let mut cursor = &data[..];
        while let Ok(Some(item)) = rustls_pemfile::read_one(&mut cursor) {
            match item {
                rustls_pemfile::Item::Pkcs1Key(_)
                | rustls_pemfile::Item::Pkcs8Key(_)
                | rustls_pemfile::Item::Sec1Key(_) => has_key = true,
                rustls_pemfile::Item::X509Certificate(_) => has_cert = true,
                _ => {}
            }
        }

        if has_key {
            key_files.push(path.clone());
        }
        if has_cert && !has_key {
            cert_files.push(path);
        }
    }

    if key_files.is_empty() {
        bail!(
            "no private key file found in {} (scanned .pem/.crt/.cert/.key files)",
            dir.display()
        );
    }
    if key_files.len() > 1 {
        let names: Vec<_> = key_files.iter().map(|p| p.display().to_string()).collect();
        bail!(
            "multiple private key files found in {}: {}",
            dir.display(),
            names.join(", ")
        );
    }
    if cert_files.is_empty() {
        bail!(
            "no certificate file found in {} (scanned .pem/.crt/.cert/.key files)",
            dir.display()
        );
    }
    if cert_files.len() > 1 {
        let names: Vec<_> = cert_files.iter().map(|p| p.display().to_string()).collect();
        bail!(
            "multiple certificate files found in {}: {} — use cert_file/key_file to disambiguate",
            dir.display(),
            names.join(", ")
        );
    }

    Ok((cert_files.remove(0), key_files.remove(0)))
}

// ---------------------------------------------------------------------------
// Certificate loading
// ---------------------------------------------------------------------------

/// Load certificate chain from a PEM file.
fn load_certs(path: &Path) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading cert file: {}", path.display()))?;
    let mut cursor = &data[..];
    let certs: Vec<_> = rustls_pemfile::certs(&mut cursor)
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("parsing certs from {}", path.display()))?;
    if certs.is_empty() {
        bail!("no certificates found in {}", path.display());
    }
    Ok(certs)
}

/// Load a private key from a PEM file (PKCS#1, PKCS#8, or SEC1).
fn load_key(path: &Path) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
    let data = std::fs::read(path)
        .with_context(|| format!("reading key file: {}", path.display()))?;
    let mut cursor = &data[..];
    while let Ok(Some(item)) = rustls_pemfile::read_one(&mut cursor) {
        match item {
            rustls_pemfile::Item::Pkcs1Key(k) => return Ok(k.into()),
            rustls_pemfile::Item::Pkcs8Key(k) => return Ok(k.into()),
            rustls_pemfile::Item::Sec1Key(k) => return Ok(k.into()),
            _ => continue,
        }
    }
    bail!("no private key found in {}", path.display());
}

/// Build a rustls `ServerConfig` from resolved cert/key/ca paths.
pub fn load_rustls_config(paths: &ResolvedPaths) -> Result<Arc<RustlsServerConfig>> {
    let certs = load_certs(&paths.cert_path)?;
    let key = load_key(&paths.key_path)?;

    let mut config = if let Some(ca_path) = &paths.ca_path {
        // mTLS: require client certificate signed by the CA.
        let ca_data = std::fs::read(ca_path)
            .with_context(|| format!("reading CA file: {}", ca_path.display()))?;
        let mut ca_cursor = &ca_data[..];
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_cursor) {
            let cert = cert.with_context(|| format!("parsing CA cert from {}", ca_path.display()))?;
            root_store.add(cert)?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .context("building mTLS client verifier")?;
        RustlsServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .context("building TLS config with mTLS")?
    } else {
        RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("building TLS config")?
    };

    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    // Enable TLS session resumption to avoid full handshakes on reconnect.
    config.session_storage = rustls::server::ServerSessionMemoryCache::new(256);

    Ok(Arc::new(config))
}

// ---------------------------------------------------------------------------
// Hot-reload via ArcSwap
// ---------------------------------------------------------------------------

/// Holds the current TLS configuration and supports hot-reload via SIGHUP.
pub struct TlsReloader {
    state: ArcSwap<Arc<RustlsServerConfig>>,
    tls_config: TlsConfig,
}

impl TlsReloader {
    pub fn new(initial: Arc<RustlsServerConfig>, tls_config: TlsConfig) -> Self {
        Self {
            state: ArcSwap::from_pointee(initial),
            tls_config,
        }
    }

    /// Get the current TLS server config.
    pub fn current(&self) -> Arc<RustlsServerConfig> {
        let guard = self.state.load();
        Arc::clone(&*guard)
    }

    /// Reload certificates from disk. Returns Ok(()) on success,
    /// Err on failure (old config remains active).
    pub fn reload(&self) -> Result<()> {
        let paths = self.tls_config.resolve_paths()?;
        let new_config = load_rustls_config(&paths)?;
        self.state.store(Arc::new(new_config));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// TLS accept loop
// ---------------------------------------------------------------------------

/// Build a hyper auto-builder once (outside the accept loop) with the
/// HTTP/2 tunables resolved from config. Shared via `Arc` so each
/// per-connection task can reuse it without reallocating settings.
fn build_http_builder(
    http_cfg: &HttpConfig,
) -> hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor> {
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http2()
        .max_concurrent_streams(Some(http_cfg.h2_max_concurrent_streams))
        .keep_alive_interval(Some(std::time::Duration::from_secs(
            http_cfg.h2_keep_alive_interval_sec,
        )))
        .keep_alive_timeout(std::time::Duration::from_secs(
            http_cfg.h2_keep_alive_timeout_sec,
        ))
        .initial_stream_window_size(http_cfg.h2_initial_stream_window)
        .initial_connection_window_size(http_cfg.h2_initial_connection_window);
    builder
}

/// Serve HTTPS connections using a manual TLS accept loop with hyper_util.
pub async fn serve_tls(
    listener: TcpListener,
    reloader: Arc<TlsReloader>,
    app: crate::NormalizedApp,
    shutdown: impl std::future::Future<Output = ()>,
    http_cfg: HttpConfig,
) -> Result<()> {
    tokio::pin!(shutdown);

    let mut join_set = JoinSet::new();
    let builder = Arc::new(build_http_builder(&http_cfg));

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("TLS listener shutting down, draining {} connections", join_set.len());
                break;
            }
            result = listener.accept() => {
                let (tcp_stream, remote_addr) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to accept TCP connection");
                        continue;
                    }
                };

                tcp_stream.set_nodelay(true).ok();

                let tls_config = reloader.current();
                let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
                let app = app.clone();
                let builder = Arc::clone(&builder);

                join_set.spawn(async move {
                    serve_tls_connection(acceptor, tcp_stream, remote_addr, app, builder).await;
                });
            }
        }
    }

    // Drain in-flight connections.
    while join_set.join_next().await.is_some() {}

    Ok(())
}

async fn serve_tls_connection(
    acceptor: tokio_rustls::TlsAcceptor,
    tcp_stream: tokio::net::TcpStream,
    remote_addr: SocketAddr,
    app: crate::NormalizedApp,
    builder: Arc<hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor>>,
) {
    let tls_stream = match acceptor.accept(tcp_stream).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(remote = %remote_addr, error = %e, "TLS handshake failed");
            return;
        }
    };

    let io = TokioIo::new(tls_stream);

    // Bridge tower::Service → hyper::Service.
    // First map Request<Incoming> → Request<axum::body::Body>, then wrap
    // in TowerToHyperService for hyper_util compatibility.
    let tower_svc = tower::ServiceBuilder::new()
        .map_request(|req: http::Request<hyper::body::Incoming>| {
            req.map(axum::body::Body::new)
        })
        .service(app);
    let service = hyper_util::service::TowerToHyperService::new(tower_svc);

    if let Err(e) = builder.serve_connection(io, service).await {
        // Ignore normal connection closures.
        let err_str = e.to_string();
        if !is_benign_connection_error(&err_str) {
            tracing::debug!(remote = %remote_addr, error = %err_str, "connection error");
        }
    }
}

fn is_benign_connection_error(err: &str) -> bool {
    err.contains("connection reset")
        || err.contains("broken pipe")
        || err.contains("connection closed")
        || err.contains("not connected")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a self-signed cert + key pair using rcgen, writing them to tempdir.
    fn create_test_cert(dir: &Path, cert_name: &str, key_name: &str) {
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key_pair)
            .unwrap();
        std::fs::write(dir.join(cert_name), cert.pem()).unwrap();
        std::fs::write(dir.join(key_name), key_pair.serialize_pem()).unwrap();
    }

    #[test]
    fn test_detect_pem_files_valid_dir() {
        let dir = tempfile::tempdir().unwrap();
        create_test_cert(dir.path(), "server.crt", "server.key");

        let (cert, key) = detect_pem_files(dir.path()).unwrap();
        assert_eq!(cert.file_name().unwrap(), "server.crt");
        assert_eq!(key.file_name().unwrap(), "server.key");
    }

    #[test]
    fn test_detect_pem_no_key_error() {
        let dir = tempfile::tempdir().unwrap();
        // Write only a cert file
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&key_pair)
            .unwrap();
        std::fs::write(dir.path().join("server.crt"), cert.pem()).unwrap();

        let result = detect_pem_files(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no private key"));
    }

    #[test]
    fn test_detect_pem_multiple_keys_error() {
        let dir = tempfile::tempdir().unwrap();
        create_test_cert(dir.path(), "server.crt", "server.key");
        // Write a second key
        let key2 = rcgen::KeyPair::generate().unwrap();
        std::fs::write(dir.path().join("backup.key"), key2.serialize_pem()).unwrap();

        let result = detect_pem_files(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("multiple private key"));
    }

    #[test]
    fn test_detect_pem_no_cert_error() {
        let dir = tempfile::tempdir().unwrap();
        // Write only a key file
        let key = rcgen::KeyPair::generate().unwrap();
        std::fs::write(dir.path().join("server.key"), key.serialize_pem()).unwrap();

        let result = detect_pem_files(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no certificate"));
    }

    #[test]
    fn test_load_valid_cert() {
        let dir = tempfile::tempdir().unwrap();
        create_test_cert(dir.path(), "server.crt", "server.key");

        let paths = ResolvedPaths {
            cert_path: dir.path().join("server.crt"),
            key_path: dir.path().join("server.key"),
            ca_path: None,
        };
        let config = load_rustls_config(&paths).unwrap();
        assert_eq!(config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }

    #[test]
    fn test_load_cert_chain() {
        // Create a CA and a server cert signed by it.
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let server_key = rcgen::KeyPair::generate().unwrap();
        let server_params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .unwrap();

        // Write chain: server cert + CA cert
        let dir = tempfile::tempdir().unwrap();
        let chain = format!("{}{}", server_cert.pem(), ca_cert.pem());
        std::fs::write(dir.path().join("chain.crt"), &chain).unwrap();
        std::fs::write(dir.path().join("server.key"), server_key.serialize_pem()).unwrap();

        let paths = ResolvedPaths {
            cert_path: dir.path().join("chain.crt"),
            key_path: dir.path().join("server.key"),
            ca_path: None,
        };
        load_rustls_config(&paths).unwrap();
    }

    #[test]
    fn test_missing_cert_file_error() {
        let dir = tempfile::tempdir().unwrap();
        create_test_cert(dir.path(), "server.crt", "server.key");

        let paths = ResolvedPaths {
            cert_path: dir.path().join("nonexistent.crt"),
            key_path: dir.path().join("server.key"),
            ca_path: None,
        };
        assert!(load_rustls_config(&paths).is_err());
    }

    #[test]
    fn test_missing_key_file_error() {
        let dir = tempfile::tempdir().unwrap();
        create_test_cert(dir.path(), "server.crt", "server.key");

        let paths = ResolvedPaths {
            cert_path: dir.path().join("server.crt"),
            key_path: dir.path().join("nonexistent.key"),
            ca_path: None,
        };
        assert!(load_rustls_config(&paths).is_err());
    }

    #[test]
    fn test_invalid_pem_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.crt"), "not a pem file").unwrap();
        std::fs::write(dir.path().join("bad.key"), "not a key").unwrap();

        let paths = ResolvedPaths {
            cert_path: dir.path().join("bad.crt"),
            key_path: dir.path().join("bad.key"),
            ca_path: None,
        };
        assert!(load_rustls_config(&paths).is_err());
    }

    #[test]
    fn test_mtls_config_with_ca() {
        // Create CA
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        // Create server cert signed by CA
        let server_key = rcgen::KeyPair::generate().unwrap();
        let server_params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
        let server_cert = server_params
            .signed_by(&server_key, &ca_cert, &ca_key)
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.crt"), server_cert.pem()).unwrap();
        std::fs::write(dir.path().join("server.key"), server_key.serialize_pem()).unwrap();
        std::fs::write(dir.path().join("ca.crt"), ca_cert.pem()).unwrap();

        let paths = ResolvedPaths {
            cert_path: dir.path().join("server.crt"),
            key_path: dir.path().join("server.key"),
            ca_path: Some(dir.path().join("ca.crt")),
        };
        let config = load_rustls_config(&paths).unwrap();
        // mTLS config should still have ALPN set
        assert_eq!(config.alpn_protocols, vec![b"h2".to_vec(), b"http/1.1".to_vec()]);
    }

    #[test]
    fn test_reload_swaps_config() {
        let dir = tempfile::tempdir().unwrap();
        create_test_cert(dir.path(), "server.crt", "server.key");

        let paths = ResolvedPaths {
            cert_path: dir.path().join("server.crt"),
            key_path: dir.path().join("server.key"),
            ca_path: None,
        };
        let initial = load_rustls_config(&paths).unwrap();

        let tls_config = TlsConfig {
            cert_dir: None,
            cert_file: Some(dir.path().join("server.crt").to_string_lossy().into_owned()),
            key_file: Some(dir.path().join("server.key").to_string_lossy().into_owned()),
            ca_file: None,
        };

        let reloader = TlsReloader::new(initial, tls_config);

        // Generate new cert + key and overwrite
        let new_key = rcgen::KeyPair::generate().unwrap();
        let new_cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .unwrap()
            .self_signed(&new_key)
            .unwrap();
        std::fs::write(dir.path().join("server.crt"), new_cert.pem()).unwrap();
        std::fs::write(dir.path().join("server.key"), new_key.serialize_pem()).unwrap();

        // Reload should succeed and the config pointer should change.
        let before = Arc::as_ptr(&reloader.current());
        reloader.reload().unwrap();
        let after = Arc::as_ptr(&reloader.current());
        assert_ne!(before, after);
    }
}
