use anyhow::{bail, Result};
use console::style;

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::{self, Config};
use crate::plugin_tracker::PluginIndex;
use crate::plugins;
use crate::store::SecretStore;

pub fn run(
    config: &Config,
    key: &str,
    env: &str,
    value: Option<&str>,
    group: Option<&str>,
    no_sync: bool,
    strict: bool,
) -> Result<()> {
    run_with_runner(
        config,
        key,
        env,
        value,
        group,
        no_sync,
        strict,
        &RealCommandRunner,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_runner(
    config: &Config,
    key: &str,
    env: &str,
    value: Option<&str>,
    group: Option<&str>,
    no_sync: bool,
    strict: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!(
            "unknown environment '{env}'. Valid: {}",
            config.environments.join(", ")
        );
    }

    // If the key isn't in config, offer to register it
    if config.find_secret(key).is_none() {
        let config_path = config.root.join("esk.yaml");
        if let Some(group) = group {
            config::add_secret_to_config(&config_path, key, group)?;
            cliclack::log::success(format!("Added '{}' to esk.yaml under {}", key, group))?;
        } else if atty::is(atty::Stream::Stdin) {
            let add =
                cliclack::confirm(format!("Secret '{}' is not in esk.yaml. Add it?", key))
                    .initial_value(true)
                    .interact()?;

            if add {
                let mut groups = config::secret_group_names(config);
                let has_groups = !groups.is_empty();

                let chosen_group = if has_groups {
                    groups.push("(New group)".to_string());
                    let options: Vec<(&str, &str, &str)> = groups
                        .iter()
                        .map(|g| (g.as_str(), g.as_str(), ""))
                        .collect();
                    let selected: &str = cliclack::select("Which group?")
                        .items(&options)
                        .interact()?;

                    if selected == "(New group)" {
                        let name: String = cliclack::input("Group name:").interact()?;
                        name
                    } else {
                        selected.to_string()
                    }
                } else {
                    let name: String = cliclack::input("Group name:").interact()?;
                    name
                };

                config::add_secret_to_config(&config_path, key, &chosen_group)?;
                cliclack::log::success(format!(
                    "Added '{}' to esk.yaml under {}",
                    key, chosen_group
                ))?;
            } else {
                cliclack::log::warning(format!("Secret '{}' is not defined in esk.yaml", key))?;
            }
        } else {
            cliclack::log::warning(format!("Secret '{}' is not defined in esk.yaml", key))?;
        }
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
        let plugin_index_path = config.root.join(".esk/plugin-index.json");
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

        if plugin_failures > 0 && strict {
            bail!(
                "{plugin_failures} plugin(s) failed to push (--strict). Adapter sync skipped.\n\
                 Fix the plugin issue, then run:\n  \
                 esk push --env {env}\n  \
                 esk sync --env {env}"
            );
        }
    }

    // Auto-sync affected targets
    crate::cli::sync::run_with_runner(config, Some(env), false, false, false, runner)?;

    if plugin_failures > 0 {
        bail!(
            "{plugin_failures} plugin(s) failed to push. Run `esk push --env {env}` to retry."
        );
    }

    Ok(())
}
