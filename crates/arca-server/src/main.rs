//! Arca S3-compatible object storage server.

mod cli;
mod cluster;
mod config;
mod connector;
mod credential;
mod compress_existing;
mod fsck;
mod recover;
mod replicator;
mod sigv4_http;
mod tls;
mod tls_generate;
mod vault;
mod worker;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use clap::Parser;
use tokio::net::TcpListener;
use tower::Layer;
use tracing_subscriber::EnvFilter;

use arca_core::store::CredentialStore;
use arca_proto::AppState;
use arca_proto::middleware::normalize::NormalizeService;
use cli::{Cli, ClusterAction, Command, CredentialAction, EncryptionAction, LogFormat, TlsAction, UserAction};

/// The normalized app type used by both HTTP and HTTPS code paths.
pub type NormalizedApp = NormalizeService<Router>;

fn main() -> Result<()> {
    // Install the `ring` rustls crypto provider as the process-wide default.
    // tonic (gRPC connector) and lettre (SMTP connector) both pull in `rustls`
    // without forcing a crypto backend, so we install one explicitly. Ignore
    // errors if it has already been set (e.g. by a dependency).
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = Cli::parse();

    // Build the tokio runtime explicitly. For `serve` we read the optional
    // [server.runtime] config to size the worker / blocking pool; the
    // remaining (short-lived CLI) subcommands use the same builder with
    // defaults so we keep one code path.
    let mut rt_builder = tokio::runtime::Builder::new_multi_thread();
    rt_builder.enable_all();
    if let Command::Serve { ref config_path, .. } = cli.command {
        if let Ok(cfg) = config::load_config(config_path) {
            if let Some(rt_cfg) = cfg.server.runtime.as_ref() {
                if rt_cfg.worker_threads > 0 {
                    rt_builder.worker_threads(rt_cfg.worker_threads);
                }
                if rt_cfg.max_blocking_threads > 0 {
                    rt_builder.max_blocking_threads(rt_cfg.max_blocking_threads);
                }
            }
        }
    }
    let rt = rt_builder.build()?;
    rt.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Serve {
            config_path,
            log_format,
        } => {
            // Load config first to get log_level, then initialize tracing.
            let config = config::load_config(&config_path)?;
            let initial_log_level = config.server.log_level.clone()
                .unwrap_or_else(|| "info".to_string());
            let log_reloader = init_tracing(&log_format, &initial_log_level);

            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                commit = env!("ARCA_GIT_COMMIT"),
                "Starting Arca"
            );

            let stores = open_stores(&config).await?;
            credential::ensure_root_credential(stores.credentials.as_ref()).await?;

            // Cluster node identity: self-assigned and persisted in server_config
            // so the TOML config stays byte-identical on every node. When the
            // cluster is enabled this also serves as the replication
            // loop-prevention source id.
            let cluster_node_id = match config.cluster.as_ref() {
                Some(c) if c.enabled => {
                    Some(cluster::identity::ensure_node_id(stores.server_config.as_ref()).await?)
                }
                _ => None,
            };

            // Shared cluster state: the peer list + write-quorum gate. Peers are
            // populated by the membership manager once spawned; this is the
            // handle AppState, the cluster endpoints, and the store decorators
            // share.
            let cluster_state = match (config.cluster.as_ref(), cluster_node_id.as_ref()) {
                (Some(c), Some(node_id)) if c.enabled => Some(std::sync::Arc::new(
                    arca_core::cluster::ClusterState::new(
                        node_id.clone(),
                        c.write_quorum(),
                        c.cluster_size,
                    ),
                )),
                _ => None,
            };

            // R4: verified inter-node TLS material ([cluster.tls]) — loaded
            // once and shared by the membership prober and the transport
            // client. Config validation makes it mandatory over HTTPS, so a
            // load failure here aborts startup (fail closed).
            let cluster_tls_material = match config
                .cluster
                .as_ref()
                .filter(|c| c.enabled)
                .and_then(|c| c.tls.as_ref())
            {
                Some(t) => Some(cluster::client::ClusterTlsMaterial::load(t)?),
                None => None,
            };

            // Start the membership manager (peer discovery + health pings) when
            // clustering is enabled. Detached task; refreshes cluster_state.
            if let (Some(c), Some(cstate)) = (config.cluster.as_ref(), cluster_state.clone()) {
                // M5: the hard floor (length, placeholders) is enforced by
                // config validation; the softer heuristic only warns, here
                // because tracing was not yet initialized at config load.
                if c.secret_looks_low_entropy() {
                    tracing::warn!(
                        "[cluster] secret looks low-entropy: prefer a random value \
                         (e.g. `openssl rand -hex 32`) — it keys ALL inter-node authentication"
                    );
                }
                let scheme = if config.server.tls.is_some() {
                    "https"
                } else {
                    "http"
                };
                let advertise_port = c.advertise_port.unwrap_or(config.server.port);
                cluster::membership::spawn(
                    c,
                    cstate,
                    scheme,
                    advertise_port,
                    cluster_tls_material.clone(),
                );
            }

            // Apply DB-stored log level if set (console setting has precedence over config file).
            if let Ok(Some(db_level)) = stores.server_config.get_server_config("log_level").await {
                if let Err(e) = log_reloader(&db_level) {
                    tracing::warn!(error = %e, level = db_level, "Failed to apply stored log level");
                } else {
                    tracing::info!(level = db_level, "Applied log level from settings");
                }
            }

            let fs_blob_store = arca_storage::FsBlobStore::new(
                config.storage.blobs_dir(),
                config.storage.blob_prefix_depth,
            )
            .await?;

            // Set up encryption stores.
            // When a master key is available (from config or KMS), we create
            // both an EncryptingBlobStore (for encrypted reads/writes) and keep the
            // plain FsBlobStore (for non-encrypted writes). This enables per-bucket
            // encryption even when the global default is off.
            let encryption_enabled = config
                .encryption
                .as_ref()
                .map(|e| e.enabled)
                .unwrap_or(false);

            // Resolve the master key: either from KMS (Vault/OpenBAO) or from config.
            let (master_key, kms_provider, kms_endpoint) = match &config.encryption {
                Some(enc) if enc.kms.is_some() => {
                    let kms = enc.kms.as_ref().unwrap();
                    let endpoint = kms.endpoint.clone();
                    let mk = vault::ensure_master_key(kms).await?;
                    (Some(mk), Some("vault".to_string()), Some(endpoint))
                }
                Some(enc) if enc.master_key.is_some() => {
                    let mk = arca_storage::encryption::keys::MasterKey::from_base64(
                        enc.master_key.as_ref().unwrap(),
                    )
                    .map_err(|e| anyhow::anyhow!("invalid master key: {e}"))?;
                    (Some(mk), Some("local".to_string()), None)
                }
                _ => (None, None, None),
            };

            // SSE-C blob store (always available).
            let mut ssec_blob: Arc<dyn arca_core::store::SsecBlobOps> =
                Arc::new(arca_storage::SsecBlobStore::new(fs_blob_store.clone()));

            let fs_arc = Arc::new(fs_blob_store.clone());

            // Stamp this node's cluster-config fingerprint (captured before the
            // master key is moved into the blob store) so peers can flag drift in
            // the alignment-critical config (cluster_id / secret / mode /
            // cluster_size / master key).
            let master_key_id: Option<String> =
                master_key.as_ref().map(|k| k.key_id().to_string());
            if let (Some(c), Some(cstate)) = (
                config.cluster.as_ref().filter(|c| c.enabled),
                cluster_state.as_ref(),
            ) {
                let mode = if c.write_quorum().is_some() {
                    "quorum"
                } else {
                    "available"
                };
                cstate.set_config_fingerprint(arca_core::cluster::config_fingerprint(
                    &c.cluster_id,
                    mode,
                    c.write_quorum(),
                    &c.secret,
                    master_key_id.as_deref(),
                ));
            }

            let (mut blob, mut plain_blob): (
                Arc<dyn arca_core::store::BlobStore>,
                Option<Arc<dyn arca_core::store::BlobStore>>,
            ) = if let Some(master_key) = master_key {
                if encryption_enabled {
                    tracing::info!(
                        key_id = master_key.key_id(),
                        provider = kms_provider.as_deref().unwrap_or("unknown"),
                        "Server-side encryption enabled (AES-256-GCM)"
                    );
                } else {
                    tracing::info!(
                        key_id = master_key.key_id(),
                        provider = kms_provider.as_deref().unwrap_or("unknown"),
                        "Encryption key configured (per-bucket encryption available)"
                    );
                }
                let plain: Arc<dyn arca_core::store::BlobStore> =
                    Arc::new(fs_blob_store.clone());
                let encrypting: Arc<dyn arca_core::store::BlobStore> =
                    Arc::new(arca_storage::EncryptingBlobStore::new(
                        fs_blob_store,
                        Arc::new(master_key),
                    ));
                (encrypting, Some(plain))
            } else {
                (Arc::new(fs_blob_store), None)
            };

            // Always wrap with transparent compression. The wrapper is a
            // per-bucket passthrough by default (no bucket config = no
            // compression); it only kicks in when a user sets
            // `PUT /{bucket}?compression` via the console or S3 API.
            // Compression always sits ABOVE encryption: we compress plaintext,
            // then encrypt the compressed bytes.
            let shared_metrics = Arc::new(arca_core::store::CompressionMetrics::new());
            let comp_blob = Arc::new(arca_storage::CompressingBlobStore::with_metrics(
                blob.clone(),
                fs_arc.clone(),
                stores.metadata.clone(),
                shared_metrics.clone(),
            ));
            let compression_invalidator: Option<Arc<dyn Fn(&str) + Send + Sync>> = {
                let inv = comp_blob.clone();
                Some(Arc::new(move |bucket: &str| inv.invalidate_bucket(bucket)))
            };
            let compression_metrics_handle = Some(shared_metrics.clone());
            blob = comp_blob as Arc<dyn arca_core::store::BlobStore>;

            if let Some(p) = plain_blob {
                let plain_comp = Arc::new(arca_storage::CompressingBlobStore::with_metrics(
                    p,
                    fs_arc.clone(),
                    stores.metadata.clone(),
                    shared_metrics.clone(),
                ));
                plain_blob = Some(plain_comp as Arc<dyn arca_core::store::BlobStore>);
            }

            let tls_enabled = config.server.tls.is_some();

            // Extract monitoring config values for AppState
            let (audit_enabled, config_audit_retention_days, metrics_enabled, config_metrics_retention_days) =
                match &config.monitoring {
                    Some(mon) => {
                        let audit_enabled = mon.audit.as_ref().map_or(true, |a| a.enabled);
                        let audit_ret = mon.audit.as_ref().and_then(|a| a.retention_days);
                        let metrics_enabled = mon.metrics.as_ref().map_or(true, |m| m.enabled);
                        let metrics_ret = mon.metrics.as_ref().and_then(|m| m.retention_days);
                        (audit_enabled, audit_ret, metrics_enabled, metrics_ret)
                    }
                    None => (true, None, true, None),
                };

            let mut metrics_registry = arca_proto::metrics::MetricsRegistry::new();
            if let Some(ref m) = compression_metrics_handle {
                metrics_registry.set_compression(m.clone());
            }
            let metrics_registry = Arc::new(metrics_registry);

            // Resolve limits config (with defaults when section is absent).
            let limits = config
                .server
                .limits
                .clone()
                .unwrap_or_default();

            // Create rate limiters (None when disabled).
            let ip_rate_limiter = arca_proto::middleware::rate_limit::create_rate_limiter(
                limits.rate_limit_per_ip_per_second,
                limits.rate_limit_per_ip_burst,
            );
            let credential_rate_limiter = arca_proto::middleware::rate_limit::create_rate_limiter(
                limits.rate_limit_per_second,
                limits.rate_limit_burst,
            );

            if ip_rate_limiter.is_some() {
                tracing::info!(
                    per_second = limits.rate_limit_per_ip_per_second,
                    burst = limits.rate_limit_per_ip_burst,
                    "Per-IP rate limiting enabled"
                );
            }
            if credential_rate_limiter.is_some() {
                tracing::info!(
                    per_second = limits.rate_limit_per_second,
                    burst = limits.rate_limit_burst,
                    "Per-credential rate limiting enabled"
                );
            }

            // Drain mode watch channel (set to true on shutdown signal).
            let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);

            // Optionally wrap metadata store with LRU cache.
            let cache_config = config.server.cache.clone().unwrap_or_default();
            let metadata: Arc<dyn arca_core::store::MetadataStore> = if cache_config.enabled {
                tracing::info!(
                    bucket_cache_size = cache_config.bucket_cache_size,
                    object_cache_size = cache_config.object_cache_size,
                    "Metadata cache enabled"
                );
                Arc::new(arca_storage::CachingMetadataStore::new(
                    stores.metadata,
                    cache_config.bucket_cache_size,
                    cache_config.bucket_cache_ttl_seconds,
                    cache_config.object_cache_size,
                    cache_config.object_cache_ttl_seconds,
                ))
            } else {
                stores.metadata
            };

            let metadata_backend = config.storage.metadata_backend.clone();

            // Cluster decorators (Phase 29 M3): when clustering is enabled, wrap
            // the blob and metadata stores so writes replicate to peers under the
            // consistency policy. They sit ABOVE caching/compression/encryption,
            // shipping already-encoded bytes and canonical rows verbatim. The
            // raw FsBlobStore (fs_arc) stays available to AppState.cluster_raw_blob
            // for the receive endpoints, so applied replicas never re-fan-out.
            // `cluster_inner` bundles the pre-decorator store handles so the
            // /cluster/v1/op receive path applies control-plane ops without
            // re-fanning them out.
            let mut cluster_inner: Option<arca_proto::ClusterInnerStores> = None;
            // Anti-entropy worker handle, kept alive for the process lifetime
            // (spawned below when clustering is enabled).
            let mut _anti_entropy_worker: Option<worker::BackgroundWorker> = None;
            // The (possibly cluster-wrapped) identity stores used by AppState.
            let mut credentials: Arc<dyn arca_core::store::CredentialStore> =
                stores.credentials.clone();
            let mut users: Arc<dyn arca_core::store::UserStore> = stores.users.clone();
            let mut grants: Arc<dyn arca_core::store::GrantStore> = stores.grants.clone();
            let mut teams: Arc<dyn arca_core::store::TeamStore> = stores.teams.clone();
            let mut server_config: Arc<dyn arca_core::store::ServerConfigStore> =
                stores.server_config.clone();
            let (blob, plain_blob, metadata): (
                Arc<dyn arca_core::store::BlobStore>,
                Option<Arc<dyn arca_core::store::BlobStore>>,
                Arc<dyn arca_core::store::MetadataStore>,
            ) = if let (Some(c), Some(cstate)) = (
                config.cluster.as_ref().filter(|c| c.enabled),
                cluster_state.clone(),
            ) {
                let node_id = cluster_node_id
                    .clone()
                    .expect("cluster node id present when clustering is enabled");
                let request_timeout =
                    std::time::Duration::from_secs(c.request_timeout_seconds.max(1));
                let client = cluster::client::ClusterClient::new(
                    node_id,
                    c.secret.clone(),
                    request_timeout,
                    cluster_tls_material.as_ref(),
                )
                .map_err(|e| anyhow::anyhow!("failed to build cluster client: {e}"))?;
                let raw: Arc<dyn arca_core::store::RawBlobOps> = fs_arc.clone();

                let cluster_blob: Arc<dyn arca_core::store::BlobStore> =
                    Arc::new(cluster::cluster_blob::ClusterBlobStore::new(
                        blob,
                        raw.clone(),
                        client.clone(),
                        cstate.clone(),
                    ));
                let cluster_plain = plain_blob.map(|p| {
                    Arc::new(cluster::cluster_blob::ClusterBlobStore::new(
                        p,
                        raw.clone(),
                        client.clone(),
                        cstate.clone(),
                    )) as Arc<dyn arca_core::store::BlobStore>
                });
                // SSE-C blobs replicate like any other (the handler's
                // write_sidecar goes through the cluster-wrapped BlobStore);
                // this wrap adds the synchronous read-repair to get_with_key
                // so a node missing the bytes serves the GET instead of
                // erroring until the next anti-entropy pass (§3.6).
                ssec_blob = Arc::new(cluster::cluster_blob::ClusterSsecBlobStore::new(
                    ssec_blob.clone(),
                    raw.clone(),
                    client.clone(),
                    cstate.clone(),
                )) as Arc<dyn arca_core::store::SsecBlobOps>;
                // Wrap the identity stores (credential/user) so their mutations
                // replicate to peers; keep the inner handles for the receive path.
                let inner_credentials = credentials.clone();
                let inner_users = users.clone();
                credentials = Arc::new(cluster::cluster_control::ClusterCredentialStore::new(
                    credentials.clone(),
                    client.clone(),
                    cstate.clone(),
                    stores.control_tombstone.clone(),
                )) as Arc<dyn arca_core::store::CredentialStore>;
                users = Arc::new(cluster::cluster_control::ClusterUserStore::new(
                    users.clone(),
                    client.clone(),
                    cstate.clone(),
                    stores.control_tombstone.clone(),
                )) as Arc<dyn arca_core::store::UserStore>;
                let inner_grants = grants.clone();
                let inner_teams = teams.clone();
                let inner_server_config = server_config.clone();
                grants = Arc::new(cluster::cluster_control::ClusterGrantStore::new(
                    grants.clone(),
                    client.clone(),
                    cstate.clone(),
                    stores.control_tombstone.clone(),
                )) as Arc<dyn arca_core::store::GrantStore>;
                teams = Arc::new(cluster::cluster_control::ClusterTeamStore::new(
                    teams.clone(),
                    client.clone(),
                    cstate.clone(),
                    stores.control_tombstone.clone(),
                )) as Arc<dyn arca_core::store::TeamStore>;
                server_config = Arc::new(cluster::cluster_control::ClusterServerConfigStore::new(
                    server_config.clone(),
                    client.clone(),
                    cstate.clone(),
                    stores.control_tombstone.clone(),
                )) as Arc<dyn arca_core::store::ServerConfigStore>;

                // Keep the inner handle for the control-plane receive path.
                let inner_metadata = metadata.clone();

                // Anti-entropy worker: periodically pulls each peer's
                // changed-since manifest and applies missing/fresher rows
                // (idempotent LWW, tombstones included), and GCs old tombstones.
                // Applies through the INNER metadata store (below the cluster
                // decorator) so reconciled rows are not re-fanned-out.
                _anti_entropy_worker = Some(cluster::anti_entropy::spawn(
                    cstate.clone(),
                    client.clone(),
                    inner_metadata.clone(),
                    raw.clone(),
                    stores.control_snapshot.clone(),
                    stores.control_tombstone.clone(),
                    std::time::Duration::from_secs(c.anti_entropy_interval_seconds.max(1)),
                    c.tombstone_grace(),
                    c.blob_repair_budget(),
                ));

                let cluster_meta: Arc<dyn arca_core::store::MetadataStore> =
                    Arc::new(cluster::cluster_meta::ClusterMetadataStore::new(
                        metadata,
                        client,
                        cstate,
                        stores.control_tombstone.clone(),
                    ));
                // Bundle the pre-decorator handles for the `/cluster/v1/op`
                // receive path (applied without re-fan-out).
                cluster_inner = Some(arca_proto::ClusterInnerStores {
                    metadata: inner_metadata,
                    credentials: inner_credentials,
                    users: inner_users,
                    grants: inner_grants,
                    teams: inner_teams,
                    server_config: inner_server_config,
                    control_snapshot: stores.control_snapshot.clone(),
                });
                tracing::info!("Cluster replication enabled (data-plane write path active)");
                (cluster_blob, cluster_plain, cluster_meta)
            } else {
                (blob, plain_blob, metadata)
            };

            let mut state = AppState {
                metadata,
                blob,
                plain_blob,
                ssec_blob: Some(ssec_blob),
                credentials,
                users,
                teams,
                grants,
                server_config,
                domain: config.server.domain.clone(),
                config_region: config.server.region.clone(),
                started_at: std::time::Instant::now(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                tls_enabled,
                audit_enabled,
                config_audit_retention_days,
                metrics_enabled,
                config_metrics_retention_days,
                config_notification_retention_days: config.notifications.as_ref().and_then(|n| n.event_retention_days),
                encryption_enabled,
                kms_provider,
                kms_endpoint,
                metadata_backend,
                data_dirs: vec![std::path::PathBuf::from(&config.storage.data_dir)],
                audit_store: if audit_enabled {
                    stores.audit.clone()
                } else {
                    None
                },
                metrics_store: if metrics_enabled {
                    stores.metrics
                } else {
                    None
                },
                metrics_registry: Some(metrics_registry),
                ip_rate_limiter,
                credential_rate_limiter,
                max_body_size: limits.max_body_size,
                max_header_count: limits.max_header_count,
                max_metadata_size: limits.max_metadata_size,
                draining: drain_rx,
                notification_tx: None,
                notification_store: stores.notification,
                connector_registry: None, // Set after building the registry below.
                presigned_url_store: stores.presigned_url,
                replication_store: stores.replication.clone(),
                replication_source_id: cluster_node_id.clone().unwrap_or_else(|| {
                    config
                        .replication
                        .as_ref()
                        .map(|r| r.source_endpoint_id.clone())
                        .unwrap_or_else(|| "arca".to_string())
                }),
                cluster: cluster_state.clone(),
                // Raw blob access + shared secret for the inter-node cluster
                // endpoints. `fs_arc` is the concrete FsBlobStore (under any
                // encryption/compression wrappers); the cluster transfers
                // already-encoded bytes verbatim, so it must bypass them.
                cluster_raw_blob: cluster_state
                    .as_ref()
                    .map(|_| fs_arc.clone() as Arc<dyn arca_core::store::RawBlobOps>),
                cluster_secret: config
                    .cluster
                    .as_ref()
                    .filter(|c| c.enabled)
                    .map(|c| c.secret.clone()),
                cluster_secret_previous: config
                    .cluster
                    .as_ref()
                    .filter(|c| c.enabled)
                    .and_then(|c| c.secret_previous.clone()),
                cluster_mtls: config
                    .cluster
                    .as_ref()
                    .filter(|c| c.enabled)
                    .is_some_and(|c| c.tls.is_some()),
                cluster_inner,
                // Only populate when the user explicitly set `journal_retention_days`
                // in TOML. The ReplicationConfig Default gives 30, so we can't distinguish
                // "user chose 30" from "not set" via the struct alone — require an explicit
                // `[replication]` section before considering it TOML-locked.
                config_replication_retention_days: config
                    .replication
                    .as_ref()
                    .map(|r| r.journal_retention_days),
                replication_journal_max_age_days: config
                    .replication
                    .as_ref()
                    .map(|r| r.journal_max_age_days)
                    .unwrap_or(90),
                bucket_encryption_cache: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                bucket_replication_cache: std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                compression_invalidator,
                audit_tx: if audit_enabled {
                    stores.audit.as_ref().map(|a| arca_proto::state::spawn_audit_writer(a.clone()))
                } else {
                    None
                },
                config_log_level: config.server.log_level.clone(),
                log_reloader: Some(log_reloader),
            };

            // Create notification channel and update state
            let notif_config = config
                .notifications
                .clone()
                .unwrap_or_default();
            let (notification_tx, notification_rx) = tokio::sync::mpsc::channel(notif_config.channel_size);
            state.notification_tx = Some(notification_tx);

            // Build the connector registry.
            let connector_registry = {
                use arca_core::s3::notification::ConnectorType;
                use arca_core::store::ConnectorRegistry;
                let mut registry = ConnectorRegistry::new();

                let webhook = connector::WebhookConnector::new(
                    std::time::Duration::from_secs(notif_config.webhook_timeout_seconds),
                );
                registry.register(ConnectorType::Webhook, Arc::new(webhook));

                let redis = connector::RedisConnector::new(
                    std::time::Duration::from_secs(notif_config.redis_timeout_seconds),
                );
                registry.register(ConnectorType::Redis, Arc::new(redis));

                let nats = connector::NatsConnector::new(
                    std::time::Duration::from_secs(notif_config.nats_timeout_seconds),
                );
                registry.register(ConnectorType::Nats, Arc::new(nats));

                let mqtt = connector::MqttConnector::new(
                    std::time::Duration::from_secs(notif_config.mqtt_timeout_seconds),
                );
                registry.register(ConnectorType::Mqtt, Arc::new(mqtt));

                let postgresql = connector::PostgresqlConnector::new(
                    std::time::Duration::from_secs(notif_config.postgresql_timeout_seconds),
                );
                registry.register(ConnectorType::Postgresql, Arc::new(postgresql));

                let mysql = connector::MysqlConnector::new(
                    std::time::Duration::from_secs(notif_config.mysql_timeout_seconds),
                );
                registry.register(ConnectorType::Mysql, Arc::new(mysql));

                let mongodb = connector::MongodbConnector::new(
                    std::time::Duration::from_secs(notif_config.mongodb_timeout_seconds),
                );
                registry.register(ConnectorType::Mongodb, Arc::new(mongodb));

                let syslog = connector::SyslogConnector::new(
                    std::time::Duration::from_secs(notif_config.syslog_timeout_seconds),
                );
                registry.register(ConnectorType::Syslog, Arc::new(syslog));

                let elasticsearch = connector::ElasticsearchConnector::new(
                    std::time::Duration::from_secs(notif_config.elasticsearch_timeout_seconds),
                );
                registry.register(ConnectorType::Elasticsearch, Arc::new(elasticsearch));

                let amqp = connector::AmqpConnector::new(
                    std::time::Duration::from_secs(notif_config.amqp_timeout_seconds),
                );
                registry.register(ConnectorType::Amqp, Arc::new(amqp));

                let kafka = connector::KafkaConnector::new(
                    std::time::Duration::from_secs(notif_config.kafka_timeout_seconds),
                );
                registry.register(ConnectorType::Kafka, Arc::new(kafka));

                let smtp = connector::SmtpConnector::new(
                    std::time::Duration::from_secs(notif_config.smtp_timeout_seconds),
                );
                registry.register(ConnectorType::Smtp, Arc::new(smtp));

                let grpc = connector::GrpcConnector::new(
                    std::time::Duration::from_secs(notif_config.grpc_timeout_seconds),
                );
                registry.register(ConnectorType::Grpc, Arc::new(grpc));

                Arc::new(registry)
            };
            state.connector_registry = Some(connector_registry.clone());

            // Spawn background workers
            let metrics_interval = config
                .monitoring
                .as_ref()
                .and_then(|m| m.metrics.as_ref())
                .map(|m| m.interval_seconds)
                .unwrap_or(60);
            let _metrics_worker = worker::spawn_metrics_worker(&state, metrics_interval);
            let _retention_worker = worker::spawn_retention_worker(&state);
            let _lifecycle_worker = worker::spawn_lifecycle_worker(
                &state,
                config.lifecycle.as_ref().map(|l| l.interval_seconds),
            );
            let _notification_worker = if let Some(ref notif_store) = state.notification_store {
                let region = state.config_region.clone().unwrap_or_else(|| "us-east-1".to_string());
                Some(worker::spawn_notification_worker(
                    notification_rx,
                    state.metadata.clone(),
                    notif_store.clone(),
                    connector_registry,
                    region,
                    notif_config,
                ))
            } else {
                None
            };

            let repl_config = config.replication.clone().unwrap_or_default();
            let _replication_worker = replicator::spawn_replication_worker(
                state.metadata.clone(),
                state.blob.clone(),
                state.replication_store.clone(),
                state.server_config.clone(),
                repl_config,
                None,
            );

            let addr = format!("{}:{}", config.server.bind, config.server.port);
            tracing::info!("Starting Arca on {addr}");

            let router = arca_proto::build_router(state);

            // NormalizeLayer must wrap the Router from outside so it runs
            // *before* Axum routing — strips trailing slashes and saves the
            // original URI in extensions for auth to verify.
            let app = arca_proto::middleware::normalize::NormalizeLayer.layer(router);

            match config.server.tls {
                None => {
                    // Plain HTTP
                    let service = {
                        use axum::ServiceExt as _;
                        app.into_make_service()
                    };

                    // Ignore SIGHUP when TLS is not enabled (default would kill the process).
                    #[cfg(unix)]
                    {
                        tokio::spawn(async {
                            let mut sighup = tokio::signal::unix::signal(
                                tokio::signal::unix::SignalKind::hangup(),
                            )
                            .expect("install SIGHUP handler");
                            loop {
                                sighup.recv().await;
                                tracing::warn!("Received SIGHUP but TLS is not enabled, ignoring (certificate reload requires TLS)");
                            }
                        });
                    }

                    let drain_timeout = std::time::Duration::from_secs(
                        limits.drain_timeout_seconds as u64,
                    );
                    let listener = TcpListener::bind(&addr).await?;
                    tracing::info!("Arca is ready (HTTP on {addr})");
                    // axum 0.8 doesn't expose the underlying hyper builder;
                    // for explicit HTTP/2 tuning on the plain path, future
                    // work should swap `axum::serve` for the manual accept
                    // loop pattern used by `tls::serve_tls`. Defaults are
                    // adequate for current workloads.
                    axum::serve(listener, service)
                        .with_graceful_shutdown(shutdown_signal(drain_tx, drain_timeout))
                        .await?;
                }
                Some(tls_config) => {
                    let paths = tls_config.resolve_paths()?;
                    // R4: the cluster CA (when clustered over HTTPS) makes the
                    // listener request — and verify — optional client certs.
                    let cluster_ca = config
                        .cluster
                        .as_ref()
                        .filter(|c| c.enabled)
                        .and_then(|c| c.tls.as_ref())
                        .map(|t| std::path::PathBuf::from(&t.ca_file));
                    let server_config = tls::load_rustls_config(&paths, cluster_ca.as_deref())?;
                    let reloader = Arc::new(tls::TlsReloader::new(
                        server_config,
                        tls_config.clone(),
                        cluster_ca,
                    ));

                    // SIGHUP handler for certificate reload.
                    #[cfg(unix)]
                    {
                        let reloader_handle = reloader.clone();
                        tokio::spawn(async move {
                            let mut sighup = tokio::signal::unix::signal(
                                tokio::signal::unix::SignalKind::hangup(),
                            )
                            .expect("install SIGHUP handler");
                            loop {
                                sighup.recv().await;
                                match reloader_handle.reload() {
                                    Ok(()) => tracing::info!("TLS certificates reloaded"),
                                    Err(e) => tracing::error!(error = %e, "TLS reload failed (keeping old config)"),
                                }
                            }
                        });
                    }

                    let drain_timeout = std::time::Duration::from_secs(
                        limits.drain_timeout_seconds as u64,
                    );
                    let listener = TcpListener::bind(&addr).await?;
                    tracing::info!("Arca is ready (HTTPS on {addr})");
                    let http_cfg = config.server.http.clone().unwrap_or_default();
                    tls::serve_tls(listener, reloader, app, shutdown_signal(drain_tx, drain_timeout), http_cfg).await?;
                }
            }

            tracing::info!("Arca stopped");
        }

        Command::Credential {
            config_path,
            action,
        } => {
            let _ = init_tracing(&LogFormat::Text, "info");

            let config = config::load_config(&config_path)?;
            let stores = open_stores(&config).await?;

            match action {
                CredentialAction::Add {
                    description,
                    admin,
                    user,
                } => {
                    // Verify the user exists.
                    let user_exists = stores.users.get_user(&user).await?.is_some();
                    if !user_exists {
                        anyhow::bail!("User \"{user}\" not found. Create the user first with `arca user create`.");
                    }
                    let cred = credential::generate_credential(&description, admin, &user);
                    stores.credentials.put_credential(&cred).await?;

                    println!("Credential created:");
                    println!("  Access Key: {}", cred.access_key_id);
                    println!("  Secret Key: {}", cred.secret_access_key);
                    println!("  Admin:      {}", if cred.admin { "yes" } else { "no" });
                    if !cred.description.is_empty() {
                        println!("  Description: {}", cred.description);
                    }
                }

                CredentialAction::List => {
                    let creds = stores.credentials.list_credentials().await?;
                    if creds.is_empty() {
                        println!("No credentials found.");
                    } else {
                        println!(
                            "{:<22} {:<10} {:<7} {:<20} {}",
                            "ACCESS KEY", "STATUS", "ROLE", "CREATED", "DESCRIPTION"
                        );
                        println!("{}", "-".repeat(81));
                        for cred in creds {
                            let status = if cred.active { "active" } else { "inactive" };
                            let role = if cred.admin { "admin" } else { "user" };
                            let created = cred.created_at.format("%Y-%m-%d %H:%M:%S");
                            println!(
                                "{:<22} {:<10} {:<7} {:<20} {}",
                                cred.access_key_id, status, role, created, cred.description
                            );
                        }
                    }
                }

                CredentialAction::Remove { access_key_id } => {
                    let deleted = stores.credentials.delete_credential(&access_key_id).await?;
                    if deleted {
                        println!("Credential {access_key_id} removed.");
                    } else {
                        println!("Credential {access_key_id} not found.");
                    }
                }
            }
        }

        Command::Recover {
            config_path,
            dry_run,
            skip_verify,
        } => {
            let _ = init_tracing(&LogFormat::Text, "info");

            let config = config::load_config(&config_path)?;
            recover::run_recover(&config, dry_run, skip_verify).await?;
        }

        Command::Fsck {
            config_path,
            verify_checksums,
        } => {
            let _ = init_tracing(&LogFormat::Text, "info");

            let config = config::load_config(&config_path)?;
            let exit_code = fsck::run_fsck(&config, verify_checksums).await?;
            std::process::exit(exit_code);
        }

        Command::CompressExisting {
            config_path,
            dry_run,
            bucket,
            algorithm,
        } => {
            let _ = init_tracing(&LogFormat::Text, "info");
            let config = config::load_config(&config_path)?;
            compress_existing::run_compress_existing(
                &config,
                dry_run,
                bucket.as_deref(),
                algorithm.as_deref(),
            )
            .await?;
        }

        Command::DecompressExisting {
            config_path,
            dry_run,
            bucket,
        } => {
            let _ = init_tracing(&LogFormat::Text, "info");
            let config = config::load_config(&config_path)?;
            compress_existing::run_decompress_existing(&config, dry_run, bucket.as_deref())
                .await?;
        }

        Command::Tls { action } => {
            match action {
                TlsAction::Generate {
                    output_dir,
                    sans,
                    days,
                } => {
                    tls_generate::generate(&output_dir, &sans, days)?;
                }
                TlsAction::GenerateCluster {
                    output_dir,
                    nodes,
                    days,
                } => {
                    tls_generate::generate_cluster(&output_dir, &nodes, days)?;
                }
            }
        }

        Command::Encryption { action } => {
            match action {
                EncryptionAction::GenerateKey => {
                    use base64::Engine;
                    let key = arca_storage::encryption::keys::generate_dek()
                        .map_err(|e| anyhow::anyhow!("key generation failed: {e}"))?;
                    let encoded = base64::engine::general_purpose::STANDARD.encode(key);
                    println!("{encoded}");
                }
            }
        }

        Command::User {
            config_path,
            action,
        } => {
            let _ = init_tracing(&LogFormat::Text, "info");

            let config = config::load_config(&config_path)?;
            let stores = open_stores(&config).await?;

            match action {
                UserAction::Create {
                    username,
                    description,
                } => {
                    // Check for duplicate username.
                    if stores.users.get_user_by_username(&username).await?.is_some() {
                        anyhow::bail!("Username \"{username}\" already exists.");
                    }

                    let user = arca_core::types::User {
                        user_id: uuid::Uuid::new_v4().to_string(),
                        username: username.clone(),
                        description,
                        is_root: false,
                        created_at: chrono::Utc::now(),
                    };

                    stores.users.put_user(&user).await?;

                    println!("User created:");
                    println!("  User ID:  {}", user.user_id);
                    println!("  Username: {}", user.username);
                    if !user.description.is_empty() {
                        println!("  Description: {}", user.description);
                    }
                }

                UserAction::List => {
                    let users = stores.users.list_users().await?;
                    if users.is_empty() {
                        println!("No users found.");
                    } else {
                        println!(
                            "{:<38} {:<20} {:<6} {:<20} {}",
                            "USER ID", "USERNAME", "ROOT", "CREATED", "DESCRIPTION"
                        );
                        println!("{}", "-".repeat(104));
                        for user in users {
                            let root = if user.is_root { "yes" } else { "no" };
                            let created = user.created_at.format("%Y-%m-%d %H:%M:%S");
                            println!(
                                "{:<38} {:<20} {:<6} {:<20} {}",
                                user.user_id, user.username, root, created, user.description
                            );
                        }
                    }
                }

                UserAction::Delete { user_id } => {
                    let user = stores.users
                        .get_user(&user_id)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("User \"{user_id}\" not found."))?;

                    if user.is_root {
                        anyhow::bail!("Cannot delete the root user.");
                    }

                    stores.users.delete_user(&user_id).await?;
                    println!("User {} ({}) deleted.", user_id, user.username);
                }
            }
        }

        Command::Cluster {
            config_path,
            action,
        } => {
            let config = config::load_config(&config_path)?;
            match action {
                ClusterAction::Status => match config.cluster.as_ref() {
                    Some(c) if c.enabled => {
                        let stores = open_stores(&config).await?;
                        let node_id =
                            cluster::identity::ensure_node_id(stores.server_config.as_ref())
                                .await?;
                        cluster::status::print_status(c, &node_id);
                    }
                    _ => {
                        println!("Clustering is not enabled in this configuration.");
                    }
                },
            }
        }
    }

    Ok(())
}

/// Holds all store trait objects, abstracting over the metadata backend.
struct StoreSet {
    metadata: Arc<dyn arca_core::store::MetadataStore>,
    credentials: Arc<dyn CredentialStore>,
    users: Arc<dyn arca_core::store::UserStore>,
    teams: Arc<dyn arca_core::store::TeamStore>,
    grants: Arc<dyn arca_core::store::GrantStore>,
    server_config: Arc<dyn arca_core::store::ServerConfigStore>,
    audit: Option<Arc<dyn arca_core::store::AuditStore>>,
    metrics: Option<Arc<dyn arca_core::store::MetricsStore>>,
    notification: Option<Arc<dyn arca_core::store::NotificationStore>>,
    presigned_url: Option<Arc<dyn arca_core::store::PresignedUrlStore>>,
    replication: Arc<dyn arca_core::store::ReplicationStore>,
    control_tombstone: Arc<dyn arca_core::store::ControlTombstoneStore>,
    control_snapshot: Arc<dyn arca_core::store::ControlSnapshotStore>,
}

/// Helper to build a `StoreSet` from any type implementing all store traits.
fn build_store_set<S>(store: Arc<S>) -> StoreSet
where
    S: arca_core::store::MetadataStore
        + CredentialStore
        + arca_core::store::UserStore
        + arca_core::store::TeamStore
        + arca_core::store::GrantStore
        + arca_core::store::ServerConfigStore
        + arca_core::store::AuditStore
        + arca_core::store::MetricsStore
        + arca_core::store::NotificationStore
        + arca_core::store::PresignedUrlStore
        + arca_core::store::ReplicationStore
        + arca_core::store::ControlTombstoneStore
        + arca_core::store::ControlSnapshotStore
        + 'static,
{
    StoreSet {
        metadata: store.clone() as Arc<dyn arca_core::store::MetadataStore>,
        credentials: store.clone() as Arc<dyn CredentialStore>,
        users: store.clone() as Arc<dyn arca_core::store::UserStore>,
        teams: store.clone() as Arc<dyn arca_core::store::TeamStore>,
        grants: store.clone() as Arc<dyn arca_core::store::GrantStore>,
        server_config: store.clone() as Arc<dyn arca_core::store::ServerConfigStore>,
        audit: Some(store.clone() as Arc<dyn arca_core::store::AuditStore>),
        metrics: Some(store.clone() as Arc<dyn arca_core::store::MetricsStore>),
        notification: Some(store.clone() as Arc<dyn arca_core::store::NotificationStore>),
        presigned_url: Some(store.clone() as Arc<dyn arca_core::store::PresignedUrlStore>),
        control_tombstone: store.clone() as Arc<dyn arca_core::store::ControlTombstoneStore>,
        control_snapshot: store.clone() as Arc<dyn arca_core::store::ControlSnapshotStore>,
        replication: store as Arc<dyn arca_core::store::ReplicationStore>,
    }
}

/// Opens the appropriate metadata store based on the config.
async fn open_stores(config: &config::Config) -> Result<StoreSet> {
    // In a cluster, hard deletes must leave tombstones (so deletions converge
    // and aren't resurrected by anti-entropy); single-node deletes outright.
    let cluster_enabled = config.cluster.as_ref().is_some_and(|c| c.enabled);
    match config.storage.metadata_backend.as_str() {
        "sqlite" => {
            let store = Arc::new(
                arca_storage::SqliteStore::open(&config.storage.db_path()).await?,
            );
            store.set_cluster_mode(cluster_enabled);
            tracing::info!(backend = "sqlite", "Metadata backend ready");
            Ok(build_store_set(store))
        }
        "postgres" => {
            let pg_config = config.storage.postgres.as_ref()
                .ok_or_else(|| anyhow::anyhow!("[storage.postgres] section required when metadata_backend = \"postgres\""))?;
            let store = Arc::new(
                arca_storage::PgStore::open(
                    &pg_config.connection_string,
                    pg_config.max_connections,
                ).await?,
            );
            store.set_cluster_mode(cluster_enabled);
            tracing::info!(backend = "postgres", "Metadata backend ready");
            Ok(build_store_set(store))
        }
        other => anyhow::bail!("Unknown metadata backend: \"{other}\""),
    }
}

/// A type-erased handle for reloading the log level filter at runtime.
pub type LogLevelReloader = std::sync::Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Initializes the tracing subscriber with a reloadable filter.
/// Returns a closure that can be called to change the log level at runtime.
fn init_tracing(format: &LogFormat, initial_level: &str) -> LogLevelReloader {
    let filter = std::env::var("ARCA_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(initial_level));

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let (filter_layer, reload_handle) = tracing_subscriber::reload::Layer::new(filter);

    match format {
        LogFormat::Text => {
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(tracing_subscriber::fmt::layer())
                .init();
        }
        LogFormat::Json => {
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(tracing_subscriber::fmt::layer().json())
                .init();
        }
    }

    std::sync::Arc::new(move |new_filter: &str| {
        let filter = EnvFilter::try_new(new_filter).map_err(|e| e.to_string())?;
        reload_handle.reload(filter).map_err(|e| e.to_string())
    })
}

/// Waits for a shutdown signal (SIGINT or SIGTERM), then enters drain mode
/// for `drain_timeout` seconds before completing.
async fn shutdown_signal(
    drain_tx: tokio::sync::watch::Sender<bool>,
    drain_timeout: std::time::Duration,
) {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        )
        .expect("install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => tracing::info!("Received SIGINT"),
            _ = sigterm.recv() => tracing::info!("Received SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.expect("install Ctrl+C handler");
        tracing::info!("Received Ctrl+C");
    }

    // Enter drain mode: health endpoint will return 503 "draining".
    let _ = drain_tx.send(true);
    tracing::info!(
        drain_seconds = drain_timeout.as_secs(),
        "Draining connections..."
    );
    tokio::time::sleep(drain_timeout).await;
    tracing::info!("Shutting down...");
}
