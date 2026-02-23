use anyhow::{bail, Result};
use console::style;

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::Config;
use crate::plugin_tracker::PluginIndex;
use crate::plugins;
use crate::store::SecretStore;

pub fn run(
    config: &Config,
    key: &str,
    env: &str,
    value: Option<&str>,
    no_sync: bool,
) -> Result<()> {
    run_with_runner(config, key, env, value, no_sync, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    key: &str,
    env: &str,
    value: Option<&str>,
    no_sync: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!(
            "unknown environment '{env}'. Valid: {}",
            config.environments.join(", ")
        );
    }

    // Validate that the key is defined in config (warn if not, but allow it)
    if config.find_secret(key).is_none() {
        cliclack::log::warning(format!("Secret '{}' is not defined in lockbox.yaml", key))?;
    }

    let secret_value = match value {
        Some(v) => v.to_string(),
        None => cliclack::password(format!("Value for {} ({})", key, env))
            .mask('*')
            .interact()?,
    };

    let store = SecretStore::open(&config.root)?;
    let payload = store.set(key, env, &secret_value)?;

    cliclack::log::success(format!("Set {}:{} (v{})", key, env, payload.version))?;

    if no_sync {
        return Ok(());
    }

    // Auto-push to all configured plugins
    let mut plugin_failures = 0u32;
    if !config.plugins.is_empty() {
        let plugin_index_path = config.root.join(".lockbox/plugin-index.json");
        let mut plugin_index = PluginIndex::load(&plugin_index_path);
        let all_plugins = plugins::build_plugins(config, runner);
        for plugin in &all_plugins {
            let spinner = cliclack::spinner();
            spinner.start(format!("Pushing → {}...", plugin.name()));
            match plugin.push(&payload, config, env) {
                Ok(()) => {
                    spinner.stop(format!(
                        "Pushed → {} {}",
                        plugin.name(),
                        style("done").green()
                    ));
                    plugin_index.record_success(plugin.name(), env, payload.version);
                }
                Err(e) => {
                    spinner.error(format!(
                        "Pushed → {} {} — {e}",
                        plugin.name(),
                        style("failed").red()
                    ));
                    plugin_index.record_failure(plugin.name(), env, payload.version, e.to_string());
                    plugin_failures += 1;
                }
            }
        }
        plugin_index.save()?;
    }

    // Auto-sync affected targets
    crate::cli::sync::run_with_runner(config, Some(env), false, false, false, runner)?;

    if plugin_failures > 0 {
        bail!(
            "{plugin_failures} plugin(s) failed to sync. Run `lockbox push --env {env}` to retry."
        );
    }

    Ok(())
}
