//! Command-line interface definition.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "arca", about = "Arca S3-compatible object storage server")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the Arca server
    Serve {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,
    },

    /// Manage S3 access credentials
    Credential {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        #[command(subcommand)]
        action: CredentialAction,
    },
}

#[derive(Subcommand)]
pub enum CredentialAction {
    /// Create a new credential
    Add {
        /// Human-readable description for this credential
        #[arg(long, default_value = "")]
        description: String,
    },

    /// List all credentials
    List,

    /// Remove a credential by access key ID
    Remove {
        /// The access key ID to remove
        access_key_id: String,
    },
}
