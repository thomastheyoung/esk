use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::generate;
use console::style;
use esk::cli::{Cli, Commands, KeyCommands};
use esk::config::Config;

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("\n {} {:#}\n", style("✖").red().bold(), e);
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    cliclack::set_theme(esk::ui::EskTheme);
    let cli = Cli::parse();

    match &cli.command {
        Commands::Delete {
            key,
            env,
            no_sync,
            strict,
        } => {
            let config = Config::find_and_load()?;
            esk::cli::delete::run(
                &config,
                &esk::cli::delete::DeleteOptions {
                    key,
                    env,
                    no_sync: *no_sync,
                    strict: *strict,
                },
            )?;
        }
        Commands::Doctor { json } => {
            let cwd = std::env::current_dir()?;
            if *json {
                esk::cli::doctor::run_json(&cwd)?;
            } else {
                esk::cli::doctor::run(&cwd)?;
            }
        }
        Commands::Diff {
            left_env,
            right_env,
            values,
        } => {
            let config = Config::find_and_load()?;
            esk::cli::diff::run(&config, left_env, right_env, *values)?;
        }
        Commands::Completions { shell } => {
            let mut command = Cli::command();
            generate(*shell, &mut command, "esk", &mut std::io::stdout());
        }
        Commands::Key { command } => match command {
            KeyCommands::Rotate => {
                let config = Config::find_and_load()?;
                esk::cli::key::rotate(&config)?;
            }
        },
        Commands::Deploy {
            env,
            force,
            dry_run,
            verbose,
            skip_validation,
            strict,
            allow_empty,
            prune,
            json,
        } => {
            let config = Config::find_and_load()?;
            let opts = esk::cli::deploy::DeployOptions {
                env: env.as_deref(),
                force: *force,
                dry_run: *dry_run,
                verbose: *verbose,
                skip_validation: *skip_validation,
                strict: *strict,
                allow_empty: *allow_empty,
                prune: *prune,
            };
            if *json {
                esk::cli::deploy::run_json(&config, &opts)?;
            } else {
                esk::cli::deploy::run(&config, &opts)?;
            }
        }
        Commands::Init { keychain } => {
            let cwd = std::env::current_dir()?;
            esk::cli::init::run(&cwd, *keychain)?;
        }
        Commands::Import { path, env, group } => {
            let config = Config::find_and_load()?;
            esk::cli::import::run(
                &config,
                &esk::cli::import::ImportOptions {
                    path,
                    env,
                    group: group.as_deref(),
                },
            )?;
        }
        Commands::Set {
            key,
            env,
            value,
            group,
            no_sync,
            strict,
            skip_validation,
            force,
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
                    strict: *strict,
                    skip_validation: *skip_validation,
                    force: *force,
                },
            )?;
        }
        Commands::Get { key, env } => {
            let config = Config::find_and_load()?;
            esk::cli::get::run(&config, key, env)?;
        }
        Commands::Run { env, app, command } => {
            let config = Config::find_and_load()?;
            return esk::cli::run::run(
                &config,
                &esk::cli::run::RunOptions {
                    env,
                    app: app.as_deref(),
                    command,
                },
            );
        }
        Commands::Generate {
            format,
            output,
            preview,
        } => {
            let config = Config::find_and_load()?;
            esk::cli::generate::run(&config, format.as_ref(), output.as_deref(), *preview)?;
        }
        Commands::LlmContext => {
            esk::cli::llm_context::run()?;
        }
        Commands::List { env, json } => {
            let config = Config::find_and_load()?;
            if *json {
                esk::cli::list::run_json(&config, env.as_deref())?;
            } else {
                esk::cli::list::run(&config, env.as_deref())?;
            }
        }
        Commands::Verify {
            env,
            target,
            all,
            json,
        } => {
            let config = Config::find_and_load()?;
            let opts = esk::cli::verify::VerifyOptions {
                env: env.as_deref(),
                target: target.as_deref(),
                all: *all,
            };
            // Returns the exit code directly: verification has four outcomes,
            // and collapsing them into ok/err would lose the distinction
            // between "found drift" and "could not look".
            return if *json {
                esk::cli::verify::run_json(&config, &opts)
            } else {
                esk::cli::verify::run(&config, &opts)
            };
        }
        Commands::Status { env, all, json } => {
            let config = Config::find_and_load()?;
            if *json {
                esk::cli::status::run_json(&config, env.as_deref(), *all)?;
            } else {
                esk::cli::status::run(&config, env.as_deref(), *all)?;
            }
        }
        Commands::Sync {
            env,
            only,
            dry_run,
            strict,
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
                    strict: *strict,
                    force: *force,
                    auto_deploy: *with_deploy,
                    prefer: *prefer,
                },
            )?;
        }
    }

    Ok(0)
}
