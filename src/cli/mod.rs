pub mod get;
pub mod init;
pub mod list;
pub mod pull;
pub mod push;
pub mod set;
pub mod status;
pub mod sync;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "lockbox",
    about = "Encrypted secrets management with multi-target sync"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize encrypted store and config
    Init,
    /// Set a secret value
    Set {
        /// Secret key name
        key: String,
        /// Environment to set for
        #[arg(long)]
        env: String,
        /// Secret value (prompts interactively if omitted)
        #[arg(long)]
        value: Option<String>,
        /// Skip auto-sync after setting
        #[arg(long)]
        no_sync: bool,
    },
    /// Retrieve a secret value
    Get {
        /// Secret key name
        key: String,
        /// Environment to retrieve from
        #[arg(long)]
        env: String,
    },
    /// List all secrets and their status
    List {
        /// Filter by environment
        #[arg(long)]
        env: Option<String>,
    },
    /// Sync secrets to configured targets
    Sync {
        /// Filter by environment
        #[arg(long)]
        env: Option<String>,
        /// Force sync even if hashes match
        #[arg(long)]
        force: bool,
        /// Show what would be synced without syncing
        #[arg(long)]
        dry_run: bool,
        /// Show detailed output
        #[arg(long, short)]
        verbose: bool,
    },
    /// Show sync status and drift
    Status {
        /// Filter by environment
        #[arg(long)]
        env: Option<String>,
        /// Show all targets including synced ones
        #[arg(long)]
        all: bool,
    },
    /// Push secrets to storage plugins
    Push {
        /// Environment to push
        #[arg(long)]
        env: String,
        /// Push to a specific plugin only
        #[arg(long)]
        only: Option<String>,
    },
    /// Pull secrets from storage plugins
    Pull {
        /// Environment to pull
        #[arg(long)]
        env: String,
        /// Pull from a specific plugin only
        #[arg(long)]
        only: Option<String>,
        /// Auto-sync after pulling
        #[arg(long)]
        sync: bool,
    },
}
