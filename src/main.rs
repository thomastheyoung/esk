mod adapters;
mod cli;
mod config;
mod reconcile;
mod store;
mod tracker;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use config::Config;

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Init => {
            let cwd = std::env::current_dir()?;
            cli::init::run(&cwd)?;
        }
        Commands::Set {
            key,
            env,
            value,
            no_sync,
        } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            cli::set::run(&config, key, env, value.as_deref(), *no_sync)?;
        }
        Commands::Get { key, env } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            cli::get::run(&config, key, env)?;
        }
        Commands::List { env } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            cli::list::run(&config, env.as_deref())?;
        }
        Commands::Sync {
            env,
            force,
            dry_run,
            verbose,
        } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            cli::sync::run(&config, env.as_deref(), *force, *dry_run, *verbose)?;
        }
        Commands::Status { env } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            cli::status::run(&config, env.as_deref())?;
        }
        Commands::Push { env } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            cli::push::run(&config, env)?;
        }
        Commands::Pull { env, sync } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            cli::pull::run(&config, env, *sync)?;
        }
    }

    Ok(())
}
