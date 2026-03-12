//! Command-line interface definition.

use clap::{Parser, Subcommand, ValueEnum};
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

        /// Log output format
        #[arg(long, default_value = "text")]
        log_format: LogFormat,
    },

    /// Manage S3 access credentials
    Credential {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        #[command(subcommand)]
        action: CredentialAction,
    },

    /// Rebuild the database from sidecar metadata files (disaster recovery)
    Recover {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        /// Print what would be recovered without modifying the database
        #[arg(long)]
        dry_run: bool,

        /// Skip MD5 checksum verification of blob files
        #[arg(long)]
        skip_verify: bool,
    },

    /// Check database and filesystem consistency
    Fsck {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        /// Verify blob checksums against stored ETags (reads every blob file)
        #[arg(long)]
        verify_checksums: bool,
    },

    /// Manage TLS certificates
    Tls {
        #[command(subcommand)]
        action: TlsAction,
    },
}

#[derive(Debug, Clone, ValueEnum)]
pub enum LogFormat {
    /// Human-readable text output (default)
    Text,
    /// Structured JSON output for log aggregation
    Json,
}

#[derive(Subcommand)]
pub enum TlsAction {
    /// Generate a self-signed CA and server certificate
    Generate {
        /// Output directory for certificate files
        #[arg(long, default_value = "/etc/arca/certs")]
        output_dir: PathBuf,
        /// Subject Alternative Names (comma-separated DNS names and IPs)
        #[arg(long, default_value = "localhost,127.0.0.1,::1")]
        sans: String,
        /// Certificate validity in days
        #[arg(long, default_value = "365")]
        days: u32,
    },
}

#[derive(Subcommand)]
pub enum CredentialAction {
    /// Create a new credential
    Add {
        /// Human-readable description for this credential
        #[arg(long, default_value = "")]
        description: String,

        /// Grant admin privileges (access to Admin API)
        #[arg(long)]
        admin: bool,
    },

    /// List all credentials
    List,

    /// Remove a credential by access key ID
    Remove {
        /// The access key ID to remove
        access_key_id: String,
    },
}
