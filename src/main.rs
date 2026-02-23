use anyhow::Result;
use clap::Parser;
use lockbox::cli::{Cli, Commands};
use lockbox::config::Config;

fn main() -> Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Delete {
            key,
            env,
            no_sync,
            strict,
        } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            lockbox::cli::delete::run(&config, key, env, *no_sync, *strict)?;
        }
        Commands::Init => {
            let cwd = std::env::current_dir()?;
            lockbox::cli::init::run(&cwd)?;
        }
        Commands::Set {
            key,
            env,
            value,
            group,
            no_sync,
            strict,
        } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            lockbox::cli::set::run(
                &config,
                key,
                env,
                value.as_deref(),
                group.as_deref(),
                *no_sync,
                *strict,
            )?;
        }
        Commands::Get { key, env } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            lockbox::cli::get::run(&config, key, env)?;
        }
        Commands::List { env } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            lockbox::cli::list::run(&config, env.as_deref())?;
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
            lockbox::cli::sync::run(&config, env.as_deref(), *force, *dry_run, *verbose)?;
        }
        Commands::Status { env, all } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            lockbox::cli::status::run(&config, env.as_deref(), *all)?;
        }
        Commands::Push { env, only } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            lockbox::cli::push::run(&config, env, only.as_deref())?;
        }
        Commands::Pull {
            env,
            only,
            sync,
            strict,
        } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            lockbox::cli::pull::run(&config, env, only.as_deref(), *sync, *strict)?;
        }
    }

    Ok(())
}
