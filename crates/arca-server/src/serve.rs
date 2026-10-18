//! HTTP and HTTPS serving: the accept loop shared by both listeners.
//!
//! Every connection gets `TCP_NODELAY`, the same hyper builder (HTTP/1.1 and
//! HTTP/2, tuned by `[server.http]`) and the same graceful shutdown: when the
//! drain period ends, every connection is told to close (an idle keep-alive
//! one at once, a busy one after its current request, HTTP/2 with `GOAWAY`),
//! and whatever is still running after [`CLOSE_GRACE`] is dropped, so a stop
//! is bounded whatever the clients do.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use hyper_util::server::conn::auto::Builder;
use hyper_util::server::graceful::{GracefulShutdown, Watcher};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use crate::config::HttpConfig;
use crate::tls::TlsReloader;

/// How long the requests still running when the drain period ends get to
/// finish; their connections are dropped after that.
pub const CLOSE_GRACE: Duration = Duration::from_secs(10);

/// What runs on top of an accepted TCP connection.
#[derive(Clone)]
pub enum Transport {
    Plain,
    /// HTTPS, with the current (hot-reloadable) certificate.
    Tls(Arc<TlsReloader>),
}

/// Serves `app` on `listener` until `shutdown` completes, then closes the
/// connections, waiting at most `close_grace` for the requests still running.
pub async fn serve(
    listener: TcpListener,
    transport: Transport,
    app: crate::ServerApp,
    shutdown: impl std::future::Future<Output = ()>,
    http_cfg: HttpConfig,
    close_grace: Duration,
) -> Result<()> {
    tokio::pin!(shutdown);

    let mut join_set = JoinSet::new();
    let builder = Arc::new(build_http_builder(&http_cfg));
    let graceful = GracefulShutdown::new();

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                tracing::info!("Listener shutting down, closing {} connections", reap_finished(&mut join_set));
                break;
            }
            result = accept(&listener) => {
                let (tcp, remote_addr) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        match accept_error_pause(&e) {
                            None => tracing::debug!(error = %e, "failed to accept TCP connection"),
                            Some(pause) => {
                                tracing::error!(error = %e, "failed to accept TCP connection, retrying in {}s", pause.as_secs());
                                tokio::time::sleep(pause).await;
                            }
                        }
                        continue;
                    }
                };
                // Taken at accept time, so a shutdown signalled while a TLS
                // handshake is still running reaches this connection too.
                let watcher = graceful.watcher();
                let (transport, app, builder) = (transport.clone(), app.clone(), Arc::clone(&builder));
                // Reaping at each accept keeps the set as large as the open
                // connections, however long the server runs.
                reap_finished(&mut join_set);
                join_set.spawn(async move {
                    serve_connection(transport, tcp, remote_addr, app, &builder, watcher).await;
                });
            }
        }
    }
    drop(listener);

    // Tell every connection to close: an idle keep-alive one closes at once,
    // a busy one after its current request (HTTP/2: GOAWAY). Whatever is
    // still running after the close grace is dropped, so shutdown is bounded.
    let closed = tokio::time::timeout(close_grace, async {
        graceful.shutdown().await;
        while join_set.join_next().await.is_some() {}
    })
    .await;
    if closed.is_err() {
        tracing::warn!(
            "{} connection(s) still busy after {}s, dropping them",
            reap_finished(&mut join_set),
            close_grace.as_secs_f32()
        );
        join_set.shutdown().await;
    }

    Ok(())
}

/// Drops the tasks of the connections that have closed and returns how many
/// are still open. A finished task stays in the set until it is joined.
fn reap_finished(connections: &mut JoinSet<()>) -> usize {
    while connections.try_join_next().is_some() {}
    connections.len()
}

/// Accepts the next connection, with `TCP_NODELAY` set. With Nagle on, a
/// response written as headers then body (every GetObject) waits for the
/// client's delayed ACK: about 40 ms per request on a keep-alive connection.
async fn accept(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    let (tcp, remote_addr) = listener.accept().await?;
    if let Err(e) = tcp.set_nodelay(true) {
        tracing::debug!(error = %e, "Failed to set TCP_NODELAY");
    }
    Ok((tcp, remote_addr))
}

/// How long to wait before accepting again after `e`. An error about the one
/// connection (reset or aborted before it was accepted) is retried at once;
/// any other, typically `EMFILE` when the process is out of file descriptors,
/// would fail again immediately, so the loop pauses instead of spinning.
fn accept_error_pause(e: &io::Error) -> Option<Duration> {
    match e.kind() {
        io::ErrorKind::ConnectionRefused | io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset => None,
        _ => Some(Duration::from_secs(1)),
    }
}

/// Builds the hyper auto-builder once (outside the accept loop) with the
/// HTTP/2 tunables resolved from config. Shared via `Arc` so each
/// per-connection task can reuse it without reallocating settings.
fn build_http_builder(http_cfg: &HttpConfig) -> Builder<TokioExecutor> {
    let mut builder = Builder::new(TokioExecutor::new());
    // hyper requires a Timer when HTTP/2 keep-alive is configured (otherwise
    // it panics at first ping with "You must supply a timer."). Installing
    // TokioTimer on both protocol builders covers H1 read timeouts and H2
    // keep-alive uniformly.
    builder.http1().timer(TokioTimer::new());
    builder
        .http2()
        .timer(TokioTimer::new())
        .max_concurrent_streams(Some(http_cfg.h2_max_concurrent_streams))
        .keep_alive_interval(Some(Duration::from_secs(http_cfg.h2_keep_alive_interval_sec)))
        .keep_alive_timeout(Duration::from_secs(http_cfg.h2_keep_alive_timeout_sec))
        .initial_stream_window_size(http_cfg.h2_initial_stream_window)
        .initial_connection_window_size(http_cfg.h2_initial_connection_window);
    builder
}

/// Sets up `transport` on an accepted connection, then serves HTTP on it.
async fn serve_connection(
    transport: Transport,
    tcp: TcpStream,
    remote_addr: SocketAddr,
    app: crate::ServerApp,
    builder: &Builder<TokioExecutor>,
    watcher: Watcher,
) {
    match transport {
        Transport::Plain => serve_http(tcp, remote_addr, app, false, builder, watcher).await,
        Transport::Tls(reloader) => {
            let acceptor = tokio_rustls::TlsAcceptor::from(reloader.current());
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::debug!(remote = %remote_addr, error = %e, "TLS handshake failed");
                    return;
                }
            };
            // Did the connection present a client certificate? The verifier
            // already validated any presented certificate against the
            // configured CA during the handshake (an invalid one fails the
            // accept above), so presence here means "CA-signed client
            // identity". Stamped on every request of the connection;
            // `cluster_auth` requires it on `/cluster/v1/*` when
            // `[cluster.tls]` is configured.
            let peer_cert_verified = tls_stream
                .get_ref()
                .1
                .peer_certificates()
                .is_some_and(|certs| !certs.is_empty());
            serve_http(tls_stream, remote_addr, app, peer_cert_verified, builder, watcher).await;
        }
    }
}

/// Serves HTTP/1.1 or HTTP/2 on `stream` until the client or the shutdown
/// closes it.
async fn serve_http<S>(
    stream: S,
    remote_addr: SocketAddr,
    app: crate::ServerApp,
    peer_cert_verified: bool,
    builder: &Builder<TokioExecutor>,
    watcher: Watcher,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Bridge tower::Service → hyper::Service.
    // First map Request<Incoming> → Request<axum::body::Body>, then wrap
    // in TowerToHyperService for hyper_util compatibility.
    let tower_svc = tower::ServiceBuilder::new()
        .map_request(move |req: http::Request<hyper::body::Incoming>| {
            let mut req = req.map(axum::body::Body::new);
            // What `into_make_service_with_connect_info` would add: the per-IP
            // rate limit and the audit log read the client address here.
            req.extensions_mut().insert(axum::extract::ConnectInfo(remote_addr));
            if peer_cert_verified {
                req.extensions_mut()
                    .insert(arca_proto::middleware::cluster_auth::ClusterPeerCertVerified);
            }
            req
        })
        .service(app);
    let service = hyper_util::service::TowerToHyperService::new(tower_svc);

    if let Err(e) = watcher.watch(builder.serve_connection(TokioIo::new(stream), service)).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ResolvedPaths, TlsConfig};

    #[derive(Clone, Copy, Debug)]
    enum Kind {
        Plain,
        Tls,
    }

    /// A `serve` on 127.0.0.1 serving `router` behind the production layers,
    /// with a client for it.
    struct TestServer {
        base: String,
        client: reqwest::Client,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<Result<()>>,
        _dir: tempfile::TempDir,
    }

    impl TestServer {
        /// Fires the shutdown signal, as the end of the drain period does.
        fn shut_down(&mut self) {
            self.stop.take().unwrap().send(()).unwrap();
        }

        /// Waits for `serve` to return, failing the test after `limit`.
        async fn stopped_within(&mut self, limit: Duration) {
            tokio::time::timeout(limit, &mut self.task)
                .await
                .expect("serve did not return in time")
                .unwrap()
                .unwrap();
        }
    }

    async fn start_server(kind: Kind, router: axum::Router, close_grace: Duration) -> TestServer {
        use tower::Layer as _;

        // reqwest needs it even for a plain-HTTP client.
        crate::crypto::ensure_default_crypto_provider();
        let dir = tempfile::tempdir().unwrap();
        let (transport, client, scheme) = match kind {
            Kind::Plain => (
                Transport::Plain,
                reqwest::Client::builder().http1_only().build().unwrap(),
                "http",
            ),
            Kind::Tls => {
                crate::tls_generate::generate_cluster(dir.path(), &["node-a=127.0.0.1".to_string()], 7)
                    .unwrap();
                let paths = ResolvedPaths {
                    cert_path: dir.path().join("node-a.crt"),
                    key_path: dir.path().join("node-a.key"),
                    ca_path: None,
                };
                let tls_config = TlsConfig {
                    cert_dir: None,
                    cert_file: Some(paths.cert_path.to_string_lossy().into_owned()),
                    key_file: Some(paths.key_path.to_string_lossy().into_owned()),
                    ca_file: None,
                };
                let initial = crate::tls::load_rustls_config(&paths, None).unwrap();
                let reloader = Arc::new(TlsReloader::new(initial, tls_config, None));
                let ca_pem = std::fs::read(dir.path().join("arca-cluster-ca.crt")).unwrap();
                let client = reqwest::Client::builder()
                    .add_root_certificate(reqwest::Certificate::from_pem(&ca_pem).unwrap())
                    .http1_only()
                    .build()
                    .unwrap();
                (Transport::Tls(reloader), client, "https")
            }
        };
        let app = arca_proto::middleware::unread_body::CloseOnUnreadBodyLayer
            .layer(arca_proto::middleware::normalize::NormalizeLayer::new(None).layer(router));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("{scheme}://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async move {
            let _ = stopped.await;
        };
        let task = tokio::spawn(serve(listener, transport, app, shutdown, HttpConfig::default(), close_grace));
        TestServer { base, client, stop: Some(stop), task, _dir: dir }
    }

    /// A route that signals `entered` when a request reaches it, then runs `then`.
    fn signalling_route<F, Fut>(entered: Arc<tokio::sync::Notify>, then: F) -> axum::Router
    where
        F: Fn() -> Fut + Clone + Send + Sync + 'static,
        Fut: std::future::Future<Output = &'static str> + Send + 'static,
    {
        axum::Router::new().route(
            "/req",
            axum::routing::get(move || {
                let (entered, then) = (entered.clone(), then.clone());
                async move {
                    entered.notify_one();
                    then().await
                }
            }),
        )
    }

    /// Idle keep-alive connections must not hold up shutdown: they are told to
    /// close as soon as the drain period ends. Waiting for them, with no close
    /// signal, made an HTTPS server with 137 idle client connections hang
    /// until systemd SIGKILLed it.
    async fn idle_keep_alive_connections_close_at_once(kind: Kind) {
        let router = axum::Router::new().route("/ok", axum::routing::get(|| async { "ok" }));
        let mut server = start_server(kind, router, Duration::from_secs(30)).await;
        let resp = server.client.get(format!("{}/ok", server.base)).send().await.unwrap();
        assert_eq!(resp.text().await.unwrap(), "ok");
        // The connection now sits idle in the client's keep-alive pool.

        server.shut_down();
        server.stopped_within(Duration::from_secs(5)).await;
    }

    /// A request already running when the drain ends completes normally.
    async fn in_flight_request_finishes(kind: Kind) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let router = signalling_route(entered.clone(), || async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        });
        let mut server = start_server(kind, router, Duration::from_secs(30)).await;
        let request = tokio::spawn(server.client.get(format!("{}/req", server.base)).send());
        entered.notified().await;

        server.shut_down();
        let resp = request.await.unwrap().unwrap();
        assert_eq!(resp.text().await.unwrap(), "done");
        server.stopped_within(Duration::from_secs(5)).await;
    }

    /// A request still running when the close grace runs out is dropped, so
    /// shutdown is bounded whatever the clients do.
    async fn requests_still_running_after_the_close_grace_are_dropped(kind: Kind) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let router = signalling_route(entered.clone(), || std::future::pending::<&'static str>());
        let grace = Duration::from_millis(300);
        let mut server = start_server(kind, router, grace).await;
        let request = tokio::spawn(server.client.get(format!("{}/req", server.base)).send());
        entered.notified().await;

        let started = std::time::Instant::now();
        server.shut_down();
        server.stopped_within(Duration::from_secs(5)).await;
        assert!(started.elapsed() >= grace, "the in-flight request gets the whole grace");
        assert!(request.await.unwrap().is_err(), "the dropped connection fails the request");
    }

    #[tokio::test]
    async fn accepted_connections_disable_nagle() {
        // Without TCP_NODELAY a response written as headers + body is held
        // back until the client's delayed ACK: ~40 ms per keep-alive GET.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (accepted, _) = accept(&listener).await.unwrap();
        assert!(accepted.nodelay().unwrap(), "accepted connection must have TCP_NODELAY");
        drop(client.await.unwrap());
    }

    /// The peer address reaches the handlers as `ConnectInfo`, on both
    /// transports: the per-IP rate limit and the audit log read it there.
    #[tokio::test]
    async fn requests_carry_the_peer_address() {
        for kind in [Kind::Plain, Kind::Tls] {
            let router = axum::Router::new().route(
                "/peer",
                axum::routing::get(|request: axum::extract::Request| async move {
                    request
                        .extensions()
                        .get::<axum::extract::ConnectInfo<SocketAddr>>()
                        .map(|info| info.0.ip().to_string())
                        .unwrap_or_default()
                }),
            );
            let mut server = start_server(kind, router, Duration::from_secs(1)).await;
            let body = server.client.get(format!("{}/peer", server.base)).send().await.unwrap().text().await.unwrap();
            assert_eq!(body, "127.0.0.1", "{kind:?}");
            server.shut_down();
            server.stopped_within(Duration::from_secs(5)).await;
        }
    }

    /// The task of a closed connection stays in the set until it is joined:
    /// never reaped, the set grew with every connection accepted since start.
    #[tokio::test]
    async fn reap_finished_drops_the_tasks_of_closed_connections() {
        let mut connections = JoinSet::new();
        for _ in 0..3 {
            connections.spawn(async {});
        }
        connections.spawn(std::future::pending::<()>());
        // The three finished tasks are reaped as soon as they have run.
        tokio::time::timeout(Duration::from_secs(5), async {
            while reap_finished(&mut connections) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finished connection tasks must be reaped");
        assert_eq!(connections.len(), 1, "only the open connection is left");
    }

    #[test]
    fn accept_pauses_only_on_errors_that_would_repeat() {
        for kind in [io::ErrorKind::ConnectionReset, io::ErrorKind::ConnectionAborted, io::ErrorKind::ConnectionRefused] {
            assert_eq!(accept_error_pause(&io::Error::from(kind)), None, "{kind:?} is about one connection");
        }
        let emfile = io::Error::from_raw_os_error(24); // EMFILE: too many open files
        assert_eq!(accept_error_pause(&emfile), Some(Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn plain_shutdown_closes_idle_keep_alive_connections_at_once() {
        idle_keep_alive_connections_close_at_once(Kind::Plain).await;
    }

    #[tokio::test]
    async fn tls_shutdown_closes_idle_keep_alive_connections_at_once() {
        idle_keep_alive_connections_close_at_once(Kind::Tls).await;
    }

    #[tokio::test]
    async fn plain_shutdown_lets_an_in_flight_request_finish() {
        in_flight_request_finishes(Kind::Plain).await;
    }

    #[tokio::test]
    async fn tls_shutdown_lets_an_in_flight_request_finish() {
        in_flight_request_finishes(Kind::Tls).await;
    }

    #[tokio::test]
    async fn plain_shutdown_drops_requests_still_running_after_the_close_grace() {
        requests_still_running_after_the_close_grace_are_dropped(Kind::Plain).await;
    }

    #[tokio::test]
    async fn tls_shutdown_drops_requests_still_running_after_the_close_grace() {
        requests_still_running_after_the_close_grace_are_dropped(Kind::Tls).await;
    }
}
