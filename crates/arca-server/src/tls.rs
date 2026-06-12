//! TLS support: certificate loading, auto-detection, hot-reload, and HTTPS serving.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arc_swap::ArcSwap;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
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

        // TD-012 resolved: PEM classification via rustls-pki-types
        // (PrivateKeyDer matches PKCS#1, PKCS#8 and SEC1 sections).
        let has_key = PrivateKeyDer::pem_slice_iter(&data).any(|item| item.is_ok());
        let has_cert = CertificateDer::pem_slice_iter(&data).any(|item| item.is_ok());

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
    let certs: Vec<_> = CertificateDer::pem_slice_iter(&data)
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
    PrivateKeyDer::from_pem_slice(&data)
        .with_context(|| format!("no private key found in {}", path.display()))
}

/// Builds a WebPki client verifier rooted at `ca_path`. With
/// `allow_unauthenticated`, a connection presenting NO certificate proceeds
/// (a presented-but-invalid certificate still fails the handshake).
fn client_verifier(
    ca_path: &Path,
    allow_unauthenticated: bool,
) -> Result<Arc<dyn rustls::server::danger::ClientCertVerifier>> {
    let ca_data = std::fs::read(ca_path)
        .with_context(|| format!("reading CA file: {}", ca_path.display()))?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(&ca_data) {
        let cert = cert.with_context(|| format!("parsing CA cert from {}", ca_path.display()))?;
        root_store.add(cert)?;
    }
    let builder = rustls::server::WebPkiClientVerifier::builder(Arc::new(root_store));
    let builder = if allow_unauthenticated {
        builder.allow_unauthenticated()
    } else {
        builder
    };
    builder.build().context("building client certificate verifier")
}

/// Build a rustls `ServerConfig` from resolved cert/key/ca paths.
///
/// `cluster_ca` is the `[cluster.tls]` CA (R4): when set — and the stricter
/// global `[tls].ca_file` is not (config validation rejects the combination) —
/// client certificates are REQUESTED and validated against it, but remain
/// optional at the TLS layer because S3 clients share this listener; the
/// `/cluster/v1/*` routes enforce presence at the route layer via the
/// `ClusterPeerCertVerified` request extension.
pub fn load_rustls_config(
    paths: &ResolvedPaths,
    cluster_ca: Option<&Path>,
) -> Result<Arc<RustlsServerConfig>> {
    crate::crypto::ensure_default_crypto_provider();
    let certs = load_certs(&paths.cert_path)?;
    let key = load_key(&paths.key_path)?;

    let mut config = if let Some(ca_path) = &paths.ca_path {
        // [tls].ca_file mTLS: client certificate REQUIRED on every connection.
        let verifier = client_verifier(ca_path, false)?;
        RustlsServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .context("building TLS config with mTLS")?
    } else if let Some(ca_path) = cluster_ca {
        // R4 cluster mTLS: optional at the TLS layer, enforced per-route.
        let verifier = client_verifier(ca_path, true)?;
        RustlsServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .context("building TLS config with cluster mTLS")?
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
    /// `[cluster.tls]` CA path (R4), kept so a SIGHUP reload rebuilds the same
    /// optional client-certificate policy.
    cluster_ca: Option<PathBuf>,
}

impl TlsReloader {
    pub fn new(
        initial: Arc<RustlsServerConfig>,
        tls_config: TlsConfig,
        cluster_ca: Option<PathBuf>,
    ) -> Self {
        Self {
            state: ArcSwap::from_pointee(initial),
            tls_config,
            cluster_ca,
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
        let new_config = load_rustls_config(&paths, self.cluster_ca.as_deref())?;
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
    // hyper requires a Timer when HTTP/2 keep-alive is configured (otherwise
    // it panics at first ping with "You must supply a timer."). Installing
    // TokioTimer on both protocol builders covers H1 read timeouts and H2
    // keep-alive uniformly.
    builder
        .http1()
        .timer(hyper_util::rt::TokioTimer::new());
    builder
        .http2()
        .timer(hyper_util::rt::TokioTimer::new())
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

    // R4: did the connection present a client certificate? The verifier
    // already validated any presented certificate against the configured CA
    // during the handshake (an invalid one fails the accept above), so
    // presence here means "CA-signed client identity". Stamped on every
    // request of the connection; `cluster_auth` requires it on
    // `/cluster/v1/*` when `[cluster.tls]` is configured.
    let peer_cert_verified = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .is_some_and(|certs| !certs.is_empty());

    let io = TokioIo::new(tls_stream);

    // Bridge tower::Service → hyper::Service.
    // First map Request<Incoming> → Request<axum::body::Body>, then wrap
    // in TowerToHyperService for hyper_util compatibility.
    let tower_svc = tower::ServiceBuilder::new()
        .map_request(move |req: http::Request<hyper::body::Incoming>| {
            let mut req = req.map(axum::body::Body::new);
            if peer_cert_verified {
                req.extensions_mut()
                    .insert(arca_proto::middleware::cluster_auth::ClusterPeerCertVerified);
            }
            req
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
        let config = load_rustls_config(&paths, None).unwrap();
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
        let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
        let server_cert = server_params
            .signed_by(&server_key, &ca_issuer)
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
        load_rustls_config(&paths, None).unwrap();
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
        assert!(load_rustls_config(&paths, None).is_err());
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
        assert!(load_rustls_config(&paths, None).is_err());
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
        assert!(load_rustls_config(&paths, None).is_err());
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
        let ca_issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
        let server_cert = server_params
            .signed_by(&server_key, &ca_issuer)
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
        let config = load_rustls_config(&paths, None).unwrap();
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
        let initial = load_rustls_config(&paths, None).unwrap();

        let tls_config = TlsConfig {
            cert_dir: None,
            cert_file: Some(dir.path().join("server.crt").to_string_lossy().into_owned()),
            key_file: Some(dir.path().join("server.key").to_string_lossy().into_owned()),
            ca_file: None,
        };

        let reloader = TlsReloader::new(initial, tls_config, None);

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

    /// R4 end-to-end over real sockets: with the cluster CA configured the
    /// listener accepts BOTH bare-TLS and client-cert connections (optional at
    /// the TLS layer, as S3 clients share the port), surfaces the verified
    /// client identity to the accept loop (the signal `serve_tls_connection`
    /// turns into the `ClusterPeerCertVerified` extension), refuses a client
    /// certificate minted by a foreign CA, and is itself refused by a client
    /// that does not trust the cluster CA — i.e. certificate verification is
    /// real in both directions (TD-015 resolved). Uses material from the
    /// SHIPPED `arca tls generate-cluster` generator.
    #[tokio::test]
    async fn cluster_mtls_optional_client_auth_end_to_end() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        crate::tls_generate::generate_cluster(
            dir.path(),
            &["node-a=127.0.0.1".to_string(), "node-b=127.0.0.1".to_string()],
            7,
        )
        .unwrap();
        let ca_path = dir.path().join("arca-cluster-ca.crt");

        // Listener: node-a's cert + optional client verification vs the CA.
        let paths = ResolvedPaths {
            cert_path: dir.path().join("node-a.crt"),
            key_path: dir.path().join("node-a.key"),
            ca_path: None,
        };
        let server_config = load_rustls_config(&paths, Some(&ca_path)).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(server_config);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Minimal HTTP/1.1 responder reporting whether the handshake carried a
        // (validated) client certificate.
        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { break };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else { return };
                    let presented = tls
                        .get_ref()
                        .1
                        .peer_certificates()
                        .is_some_and(|c| !c.is_empty());
                    let mut buf = [0u8; 4096];
                    let _ = tls.read(&mut buf).await;
                    let body = if presented { "cert" } else { "nocert" };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = tls.write_all(resp.as_bytes()).await;
                    let _ = tls.shutdown().await;
                });
            }
        });

        let url = format!("https://127.0.0.1:{}/", addr.port());
        let ca_pem = std::fs::read(&ca_path).unwrap();
        let ca = || reqwest::Certificate::from_pem(&ca_pem).unwrap();

        // (1) No client cert: the connection succeeds and the server sees no
        // client identity (this is every S3 client).
        let plain = reqwest::Client::builder()
            .add_root_certificate(ca())
            .http1_only()
            .build()
            .unwrap();
        let body = plain.get(&url).send().await.unwrap().text().await.unwrap();
        assert_eq!(body, "nocert");

        // (2) node-b's CA-signed identity: accepted, identity visible.
        let mut identity_pem = std::fs::read(dir.path().join("node-b.crt")).unwrap();
        identity_pem.push(b'\n');
        identity_pem.extend_from_slice(&std::fs::read(dir.path().join("node-b.key")).unwrap());
        let with_cert = reqwest::Client::builder()
            .add_root_certificate(ca())
            .identity(reqwest::Identity::from_pem(&identity_pem).unwrap())
            .http1_only()
            .build()
            .unwrap();
        let body = with_cert.get(&url).send().await.unwrap().text().await.unwrap();
        assert_eq!(body, "cert");

        // (3) A client identity minted by a FOREIGN CA fails the handshake —
        // presented-but-invalid is rejected even though presence is optional.
        let foreign = tempfile::tempdir().unwrap();
        crate::tls_generate::generate_cluster(
            foreign.path(),
            &["rogue=127.0.0.1".to_string()],
            7,
        )
        .unwrap();
        let mut rogue_pem = std::fs::read(foreign.path().join("rogue.crt")).unwrap();
        rogue_pem.push(b'\n');
        rogue_pem.extend_from_slice(&std::fs::read(foreign.path().join("rogue.key")).unwrap());
        let rogue = reqwest::Client::builder()
            .add_root_certificate(ca())
            .identity(reqwest::Identity::from_pem(&rogue_pem).unwrap())
            .http1_only()
            .build()
            .unwrap();
        assert!(
            rogue.get(&url).send().await.is_err(),
            "a foreign-CA client certificate must fail the handshake"
        );

        // (4) A client that does NOT trust the cluster CA refuses the server
        // certificate — outbound verification is really on (TD-015).
        let untrusting = reqwest::Client::builder().http1_only().build().unwrap();
        assert!(
            untrusting.get(&url).send().await.is_err(),
            "the server certificate must not verify without the cluster CA"
        );
    }
}
