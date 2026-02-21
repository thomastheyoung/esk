use anyhow::{bail, Result};
use console::style;
use dialoguer::Password;

use crate::adapters::onepassword::OnePasswordAdapter;
use crate::adapters::RealCommandRunner;
use crate::config::Config;
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

    // Auto-push to 1Password if configured
    if let Some(op_config) = &config.adapters.onepassword {
        let runner = RealCommandRunner;
        let adapter = OnePasswordAdapter {
            config,
            adapter_config: op_config,
            runner: &runner,
        };

        let suffix = format!(":{env}");
        let env_secrets = payload
            .secrets
            .iter()
            .filter_map(|(k, v)| {
                k.strip_suffix(&suffix)
                    .map(|bare| (bare.to_string(), v.clone()))
            })
            .collect();

        let item_name = config.onepassword_item_name(env)?;
        print!("  {} 1Password ({})...", style("pushing").cyan(), item_name);
        match adapter.push_item(env, &env_secrets, payload.version) {
            Ok(()) => println!(" {}", style("done").green()),
            Err(e) => {
                println!(" {}", style("failed").red());
                eprintln!("  {e}");
            }
        }
    }

    // Auto-sync affected targets
    println!();
    crate::cli::sync::run(config, Some(env), false, false, false)?;

    Ok(())
}
