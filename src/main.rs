use anyhow::Result;
use clap::Parser;
use console::style;
use esk::cli::{Cli, Commands};
use esk::config::Config;

fn main() {
    if let Err(e) = run() {
        eprintln!("\n {} {:#}\n", style("✖").red().bold(), e);
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
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
            esk::cli::delete::run(&config, key, env, *no_sync, *strict)?;
        }
        Commands::Deploy {
            env,
            force,
            dry_run,
            verbose,
        } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            esk::cli::deploy::run(&config, env.as_deref(), *force, *dry_run, *verbose)?;
        }
        Commands::Init => {
            let cwd = std::env::current_dir()?;
            esk::cli::init::run(&cwd)?;
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
            esk::cli::set::run(
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
            esk::cli::get::run(&config, key, env)?;
        }
        Commands::Generate { runtime, output } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            esk::cli::generate::run(&config, *runtime, output.as_deref())?;
        }
        Commands::List { env } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            esk::cli::list::run(&config, env.as_deref())?;
        }
        Commands::Status { env, all } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            esk::cli::status::run(&config, env.as_deref(), *all)?;
        }
        Commands::Sync {
            env,
            only,
            dry_run,
            no_partial,
            force,
            with_deploy,
            prefer,
        } => {
            let cwd = std::env::current_dir()?;
            let config_path = Config::find(&cwd)?;
            let config = Config::load(&config_path)?;
            esk::cli::sync::run(
                &config,
                esk::cli::sync::SyncOptions {
                    env: env.as_deref(),
                    only: only.as_deref(),
                    dry_run: *dry_run,
                    no_partial: *no_partial,
                    force: *force,
                    auto_deploy: *with_deploy,
                    prefer: *prefer,
                },
            )?;
        }
    }

    Ok(())
}
