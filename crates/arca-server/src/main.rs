//! Arca S3-compatible object storage server.

mod cli;
mod config;
mod connector;
mod credential;
mod fsck;
mod recover;
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
use cli::{Cli, Command, CredentialAction, EncryptionAction, LogFormat, TlsAction, UserAction};

/// The normalized app type used by both HTTP and HTTPS code paths.
pub type NormalizedApp = NormalizeService<Router>;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            config_path,
            log_format,
        } => {
            // Initialize tracing with the requested format.
            init_tracing(&log_format);

            let config = config::load_config(&config_path)?;

            let stores = open_stores(&config).await?;
            credential::ensure_root_credential(stores.credentials.as_ref()).await?;

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
            let ssec_blob: Arc<dyn arca_core::store::SsecBlobOps> =
                Arc::new(arca_storage::SsecBlobStore::new(fs_blob_store.clone()));

            let (blob, plain_blob): (Arc<dyn arca_core::store::BlobStore>, Option<Arc<dyn arca_core::store::BlobStore>>) =
                if let Some(master_key) = master_key {
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
                    let plain = Arc::new(fs_blob_store.clone());
                    let encrypting = Arc::new(arca_storage::EncryptingBlobStore::new(
                        fs_blob_store,
                        Arc::new(master_key),
                    ));
                    (encrypting, Some(plain))
                } else {
                    (Arc::new(fs_blob_store), None)
                };

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

            let metrics_registry = Arc::new(arca_proto::metrics::MetricsRegistry::new());

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

            let mut state = AppState {
                metadata,
                blob,
                plain_blob,
                ssec_blob: Some(ssec_blob),
                credentials: stores.credentials,
                users: stores.users,
                teams: stores.teams,
                grants: stores.grants,
                server_config: stores.server_config,
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
                    stores.audit
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
            let _lifecycle_worker = worker::spawn_lifecycle_worker(&state, None);
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
                    axum::serve(listener, service)
                        .with_graceful_shutdown(shutdown_signal(drain_tx, drain_timeout))
                        .await?;
                }
                Some(tls_config) => {
                    let paths = tls_config.resolve_paths()?;
                    let server_config = tls::load_rustls_config(&paths)?;
                    let reloader = Arc::new(tls::TlsReloader::new(server_config, tls_config.clone()));

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
                    tls::serve_tls(listener, reloader, app, shutdown_signal(drain_tx, drain_timeout)).await?;
                }
            }

            tracing::info!("Arca stopped");
        }

        Command::Credential {
            config_path,
            action,
        } => {
            init_tracing(&LogFormat::Text);

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
            init_tracing(&LogFormat::Text);

            let config = config::load_config(&config_path)?;
            recover::run_recover(&config, dry_run, skip_verify).await?;
        }

        Command::Fsck {
            config_path,
            verify_checksums,
        } => {
            init_tracing(&LogFormat::Text);

            let config = config::load_config(&config_path)?;
            let exit_code = fsck::run_fsck(&config, verify_checksums).await?;
            std::process::exit(exit_code);
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
            init_tracing(&LogFormat::Text);

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
        presigned_url: Some(store as Arc<dyn arca_core::store::PresignedUrlStore>),
    }
}

/// Opens the appropriate metadata store based on the config.
async fn open_stores(config: &config::Config) -> Result<StoreSet> {
    match config.storage.metadata_backend.as_str() {
        "sqlite" => {
            let store = Arc::new(
                arca_storage::SqliteStore::open(&config.storage.db_path()).await?,
            );
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
            tracing::info!(backend = "postgres", "Metadata backend ready");
            Ok(build_store_set(store))
        }
        other => anyhow::bail!("Unknown metadata backend: \"{other}\""),
    }
}

/// Initializes the tracing subscriber with the requested log format.
fn init_tracing(format: &LogFormat) {
    let filter = std::env::var("ARCA_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    match format {
        LogFormat::Text => {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .init();
        }
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(filter)
                .init();
        }
    }
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
