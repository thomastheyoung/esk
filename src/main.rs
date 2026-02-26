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
    cliclack::set_theme(esk::ui::EskTheme);
    let cli = Cli::parse();

    match &cli.command {
        Commands::Delete {
            key,
            env,
            no_sync,
            bail,
        } => {
            let config = Config::find_and_load()?;
            esk::cli::delete::run(&config, key, env, *no_sync, *bail)?;
        }
        Commands::Deploy {
            env,
            force,
            dry_run,
            verbose,
            skip_validation,
            skip_requirements,
        } => {
            let config = Config::find_and_load()?;
            esk::cli::deploy::run(
                &config,
                &esk::cli::deploy::DeployOptions {
                    env: env.as_deref(),
                    force: *force,
                    dry_run: *dry_run,
                    verbose: *verbose,
                    skip_validation: *skip_validation,
                    skip_requirements: *skip_requirements,
                },
            )?;
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
            bail,
            skip_validation,
        } => {
            let config = Config::find_and_load()?;
            esk::cli::set::run(
                &config,
                &esk::cli::set::SetOptions {
                    key,
                    env,
                    value: value.as_deref(),
                    group: group.as_deref(),
                    no_sync: *no_sync,
                    bail: *bail,
                    skip_validation: *skip_validation,
                },
            )?;
        }
        Commands::Get { key, env } => {
            let config = Config::find_and_load()?;
            esk::cli::get::run(&config, key, env)?;
        }
        Commands::Generate { runtime, output } => {
            let config = Config::find_and_load()?;
            esk::cli::generate::run(&config, *runtime, output.as_deref())?;
        }
        Commands::List { env } => {
            let config = Config::find_and_load()?;
            esk::cli::list::run(&config, env.as_deref())?;
        }
        Commands::Status { env, all } => {
            let config = Config::find_and_load()?;
            esk::cli::status::run(&config, env.as_deref(), *all)?;
        }
        Commands::Sync {
            env,
            only,
            dry_run,
            bail,
            force,
            with_deploy,
            prefer,
        } => {
            let config = Config::find_and_load()?;
            esk::cli::sync::run(
                &config,
                esk::cli::sync::SyncOptions {
                    env: env.as_deref(),
                    only: only.as_deref(),
                    dry_run: *dry_run,
                    bail: *bail,
                    force: *force,
                    auto_deploy: *with_deploy,
                    prefer: *prefer,
                },
            )?;
        }
    }

    Ok(())
}
