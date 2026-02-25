pub mod delete;
pub mod deploy;
pub mod generate;
pub mod get;
pub mod init;
pub mod list;
pub mod set;
pub mod status;
pub mod sync;

use clap::{Parser, Subcommand};

use crate::reconcile::ConflictPreference;

#[derive(Parser)]
#[command(
    name = "esk",
    about = "Encrypted secrets management with multi-target sync"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Delete a secret value
    Delete {
        /// Secret key name
        key: String,
        /// Environment to delete from
        #[arg(long)]
        env: String,
        /// Skip auto-sync after deleting
        #[arg(long)]
        no_sync: bool,
        /// Fail if any remote push fails (skip target deploy)
        #[arg(long)]
        strict: bool,
    },
    /// Deploy secrets to configured targets
    Deploy {
        /// Filter by environment
        #[arg(long)]
        env: Option<String>,
        /// Force deploy even if hashes match
        #[arg(long)]
        force: bool,
        /// Show what would be deployed without deploying
        #[arg(long)]
        dry_run: bool,
        /// Show detailed output
        #[arg(long, short)]
        verbose: bool,
    },
    /// Initialize encrypted store and config
    Init,
    /// Set a secret value
    Set {
        /// Secret key name
        key: String,
        /// Environment to set for
        #[arg(long)]
        env: String,
        /// Secret value (WARNING: visible in process list; omit for interactive prompt)
        #[arg(long)]
        value: Option<String>,
        /// Config group to register the secret under (skips interactive prompt)
        #[arg(long)]
        group: Option<String>,
        /// Skip auto-sync after setting
        #[arg(long)]
        no_sync: bool,
        /// Fail if any remote push fails (skip target deploy)
        #[arg(long)]
        strict: bool,
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
    /// Show sync status and drift
    Status {
        /// Filter by environment
        #[arg(long)]
        env: Option<String>,
        /// Show all targets including synced ones
        #[arg(long)]
        all: bool,
    },
    /// Generate TypeScript type declarations for secrets
    Generate {
        /// Generate runtime validator instead of .d.ts
        #[arg(long)]
        runtime: bool,
        /// Output file path (defaults to env.d.ts or env.ts)
        #[arg(long, short)]
        output: Option<String>,
    },
    /// Sync secrets with remotes (pull, reconcile, push)
    Sync {
        /// Environment to sync (omit to sync all)
        #[arg(long)]
        env: Option<String>,
        /// Sync a specific remote only
        #[arg(long)]
        only: Option<String>,
        /// Show what would change without modifying anything
        #[arg(long)]
        dry_run: bool,
        /// Fail if any remote is unreachable (no partial reconciliation)
        #[arg(long = "no-partial", alias = "strict")]
        no_partial: bool,
        /// Bypass version jump protection (use with caution)
        #[arg(long)]
        force: bool,
        /// Auto-deploy targets after syncing
        #[arg(long = "with-deploy", alias = "deploy")]
        with_deploy: bool,
        /// When versions match but content differs, prefer this side
        #[arg(long, value_enum, default_value_t = ConflictPreference::Local)]
        prefer: ConflictPreference,
    },
}
