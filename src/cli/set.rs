use anyhow::{bail, Result};
use console::style;
use dialoguer::Password;

use crate::adapters::RealCommandRunner;
use crate::config::Config;
use crate::plugins;
use crate::store::SecretStore;

pub fn run(
    config: &Config,
    key: &str,
    env: &str,
    value: Option<&str>,
    no_sync: bool,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!(
            "unknown environment '{env}'. Valid: {}",
            config.environments.join(", ")
        );
    }

    // Validate that the key is defined in config (warn if not, but allow it)
    if config.find_secret(key).is_none() {
        eprintln!(
            "  {} secret '{key}' is not defined in lockbox.yaml",
            style("warning:").yellow()
        );
    }

    let secret_value = match value {
        Some(v) => v.to_string(),
        None => Password::new()
            .with_prompt(format!("Value for {key} ({env})"))
            .interact()?,
    };

    let store = SecretStore::open(&config.root)?;
    let payload = store.set(key, env, &secret_value)?;

    println!(
        "  {} {}:{} (v{})",
        style("set").green(),
        key,
        env,
        payload.version
    );

    if no_sync {
        return Ok(());
    }

    // Auto-push to all configured plugins
    if !config.plugins.is_empty() {
        let runner = RealCommandRunner;
        let all_plugins = plugins::build_plugins(config, &runner);
        for plugin in &all_plugins {
            print!("  {} {}...", style("pushing").cyan(), plugin.name());
            match plugin.push(&payload, config, env) {
                Ok(()) => println!(" {}", style("done").green()),
                Err(e) => {
                    println!(" {}", style("failed").red());
                    eprintln!("  {e}");
                }
            }
        }
    }

    // Auto-sync affected targets
    println!();
    crate::cli::sync::run(config, Some(env), false, false, false)?;

    Ok(())
}
