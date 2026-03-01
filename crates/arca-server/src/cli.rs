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
}
