//! Arca S3-compatible object storage server.

mod cli;
mod config;
mod credential;
mod fsck;
mod recover;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tower::Layer;
use tracing_subscriber::EnvFilter;

use arca_core::store::CredentialStore;
use arca_proto::AppState;
use cli::{Cli, Command, CredentialAction, LogFormat};

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

            let blob_store = arca_storage::FsBlobStore::new(
                config.storage.blobs_dir(),
                config.storage.blob_prefix_depth,
            )
            .await?;

            let state = AppState {
                metadata: store.clone() as Arc<dyn arca_core::store::MetadataStore>,
                blob: Arc::new(blob_store),
                credentials: store as Arc<dyn CredentialStore>,
                domain: config.server.domain.clone(),
            };

            let addr = format!("{}:{}", config.server.bind, config.server.port);
            tracing::info!("Starting Arca on {addr}");

            let router = arca_proto::build_router(state);

            // NormalizeLayer must wrap the Router from outside so it runs
            // *before* Axum routing — strips trailing slashes and saves the
            // original URI in extensions for auth to verify.
            let app = arca_proto::middleware::normalize::NormalizeLayer.layer(router);
            let service = {
                use axum::ServiceExt as _;
                app.into_make_service()
            };

            let listener = TcpListener::bind(&addr).await?;
            tracing::info!("Arca is ready");
            axum::serve(listener, service)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
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
                CredentialAction::Add { description } => {
                    let cred = credential::generate_credential(&description);
                    store.put_credential(&cred).await?;

                    println!("Credential created:");
                    println!("  Access Key: {}", cred.access_key_id);
                    println!("  Secret Key: {}", cred.secret_access_key);
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
                            "{:<22} {:<10} {:<20} {}",
                            "ACCESS KEY", "STATUS", "CREATED", "DESCRIPTION"
                        );
                        println!("{}", "-".repeat(72));
                        for cred in creds {
                            let status = if cred.active { "active" } else { "inactive" };
                            let created = cred.created_at.format("%Y-%m-%d %H:%M:%S");
                            println!(
                                "{:<22} {:<10} {:<20} {}",
                                cred.access_key_id, status, created, cred.description
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
    }

    Ok(())
}

/// Initializes the tracing subscriber with the requested log format.
fn init_tracing(format: &LogFormat) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
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
