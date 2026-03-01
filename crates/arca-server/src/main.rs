//! Arca S3-compatible object storage server.

mod cli;
mod config;

use anyhow::Result;
use clap::Parser;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Command};

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

            let addr = format!("{}:{}", config.server.bind, config.server.port);
            tracing::info!("Starting Arca on {addr}");

            let router = arca_proto::build_router();
            let listener = TcpListener::bind(&addr).await?;

            tracing::info!("Arca is ready");
            axum::serve(listener, router).await?;
        }
    }

    Ok(())
}
