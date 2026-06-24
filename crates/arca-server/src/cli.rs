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

    /// Manage server-side encryption
    Encryption {
        #[command(subcommand)]
        action: EncryptionAction,
    },

    /// Compress existing blobs in place (offline, atomic, resumable)
    CompressExisting {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        /// Only report what would change; no files written
        #[arg(long)]
        dry_run: bool,

        /// Restrict to a single bucket
        #[arg(long)]
        bucket: Option<String>,

        /// Override the default algorithm (auto|zstd|lz4|snappy|gzip|brotli|xz)
        #[arg(long)]
        algorithm: Option<String>,
    },

    /// Decompress previously compressed blobs back to plaintext on disk
    DecompressExisting {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        /// Only report what would change; no files written
        #[arg(long)]
        dry_run: bool,

        /// Restrict to a single bucket
        #[arg(long)]
        bucket: Option<String>,
    },

    /// Encrypt existing plaintext blobs in place (offline, atomic, idempotent)
    EncryptExisting {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        /// Only report what would change; no files or rows written
        #[arg(long)]
        dry_run: bool,

        /// Restrict to a single bucket
        #[arg(long)]
        bucket: Option<String>,

        /// Restrict to keys with this prefix
        #[arg(long)]
        prefix: Option<String>,
    },

    /// Decrypt existing SSE-S3 blobs back to plaintext in place (offline, atomic, idempotent)
    DecryptExisting {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        /// Only report what would change; no files or rows written
        #[arg(long)]
        dry_run: bool,

        /// Restrict to a single bucket
        #[arg(long)]
        bucket: Option<String>,

        /// Restrict to keys with this prefix
        #[arg(long)]
        prefix: Option<String>,
    },

    /// Manage users (offline, direct database access)
    User {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        #[command(subcommand)]
        action: UserAction,
    },

    /// Inspect the HA cluster (this node's identity, peers, quorum)
    Cluster {
        /// Path to the configuration file
        #[arg(long, default_value = "/etc/arca/config.toml")]
        config_path: PathBuf,

        #[command(subcommand)]
        action: ClusterAction,
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

    /// Generate a cluster CA + per-node certificates for inter-node mTLS ([cluster.tls])
    GenerateCluster {
        /// Output directory for certificate files
        #[arg(long, default_value = "/etc/arca/certs/cluster")]
        output_dir: PathBuf,
        /// One per node: `name` or `name=san1,san2,...` — the SANs must cover
        /// every DNS name/IP peers use to reach the node (seeds entries,
        /// advertised addresses); they default to the name itself
        #[arg(long = "node", required = true)]
        nodes: Vec<String>,
        /// Node certificate validity in days (the CA lasts twice as long)
        #[arg(long, default_value = "365")]
        days: u32,
    },
}

#[derive(Subcommand)]
pub enum EncryptionAction {
    /// Generate a random 256-bit master key (base64-encoded)
    GenerateKey,
}

#[derive(Subcommand)]
pub enum UserAction {
    /// Create a new user
    Create {
        /// Username (must be unique)
        username: String,

        /// Human-readable description
        #[arg(long, default_value = "")]
        description: String,
    },

    /// List all users
    List,

    /// Delete a user by user ID
    Delete {
        /// The user ID to delete
        user_id: String,
    },
}

#[derive(Subcommand)]
pub enum ClusterAction {
    /// Show this node's identity and the current cluster topology
    Status,
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

        /// User ID to associate the credential with (default: root)
        #[arg(long, default_value = "root")]
        user: String,
    },

    /// List all credentials
    List,

    /// Remove a credential by access key ID
    Remove {
        /// The access key ID to remove
        access_key_id: String,
    },
}
