//! Arca S3-compatible object storage server.

mod cli;
mod config;
mod credential;
mod fsck;
mod recover;
mod tls;
mod tls_generate;

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
use cli::{Cli, Command, CredentialAction, EncryptionAction, LogFormat, TlsAction};

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

            let store = Arc::new(
                arca_storage::SqliteStore::open(&config.storage.db_path()).await?,
            );
            credential::ensure_root_credential(store.as_ref()).await?;

            let fs_blob_store = arca_storage::FsBlobStore::new(
                config.storage.blobs_dir(),
                config.storage.blob_prefix_depth,
            )
            .await?;

            // Conditionally wrap with EncryptingBlobStore when encryption is enabled.
            let encryption_enabled = config
                .encryption
                .as_ref()
                .map(|e| e.enabled)
                .unwrap_or(false);

            let blob: Arc<dyn arca_core::store::BlobStore> = if encryption_enabled {
                let enc_config = config.encryption.as_ref().unwrap();
                let master_key = arca_storage::encryption::keys::MasterKey::from_base64(
                    enc_config.master_key.as_ref().unwrap(),
                )
                .map_err(|e| anyhow::anyhow!("invalid master key: {e}"))?;
                tracing::info!(
                    key_id = master_key.key_id(),
                    "Server-side encryption enabled (AES-256-GCM)"
                );
                Arc::new(arca_storage::EncryptingBlobStore::new(
                    fs_blob_store,
                    Arc::new(master_key),
                ))
            } else {
                Arc::new(fs_blob_store)
            };

            let tls_enabled = config.server.tls.is_some();

            let state = AppState {
                metadata: store.clone() as Arc<dyn arca_core::store::MetadataStore>,
                blob,
                credentials: store as Arc<dyn CredentialStore>,
                domain: config.server.domain.clone(),
                started_at: std::time::Instant::now(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                tls_enabled,
                encryption_enabled,
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

                    let listener = TcpListener::bind(&addr).await?;
                    tracing::info!("Arca is ready (HTTP on {addr})");
                    axum::serve(listener, service)
                        .with_graceful_shutdown(shutdown_signal())
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

                    let listener = TcpListener::bind(&addr).await?;
                    tracing::info!("Arca is ready (HTTPS on {addr})");
                    tls::serve_tls(listener, reloader, app, shutdown_signal()).await?;
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
            let store = arca_storage::SqliteStore::open(&config.storage.db_path()).await?;

            match action {
                CredentialAction::Add { description, admin } => {
                    let cred = credential::generate_credential(&description, admin);
                    store.put_credential(&cred).await?;

                    println!("Credential created:");
                    println!("  Access Key: {}", cred.access_key_id);
                    println!("  Secret Key: {}", cred.secret_access_key);
                    println!("  Admin:      {}", if cred.admin { "yes" } else { "no" });
                    if !cred.description.is_empty() {
                        println!("  Description: {}", cred.description);
                    }
                }

                CredentialAction::List => {
                    let creds = store.list_credentials().await?;
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
                    let deleted = store.delete_credential(&access_key_id).await?;
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
    }

    Ok(())
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

/// Waits for a shutdown signal (SIGINT or SIGTERM).
async fn shutdown_signal() {
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

    tracing::info!("Shutting down gracefully...");
}
