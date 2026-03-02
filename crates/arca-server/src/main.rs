//! Arca S3-compatible object storage server.

mod cli;
mod config;
mod credential;

use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use arca_core::store::CredentialStore;
use arca_proto::AppState;
use cli::{Cli, Command, CredentialAction};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Serve { config_path } => {
            let config = config::load_config(&config_path)?;

            let store = arca_storage::SqliteStore::open(&config.storage.db_path()).await?;
            credential::ensure_root_credential(&store).await?;

            let blob_store = arca_storage::FsBlobStore::new(
                config.storage.blobs_dir(),
                config.storage.blob_prefix_depth,
            )
            .await?;

            let state = AppState {
                metadata: Arc::new(store),
                blob: Arc::new(blob_store),
            };

            let addr = format!("{}:{}", config.server.bind, config.server.port);
            tracing::info!("Starting Arca on {addr}");

            let router = arca_proto::build_router(state);
            let listener = TcpListener::bind(&addr).await?;

            tracing::info!("Arca is ready");
            axum::serve(listener, router).await?;
        }

        Command::Credential {
            config_path,
            action,
        } => {
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
    }

    Ok(())
}
