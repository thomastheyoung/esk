use anyhow::{bail, Result};

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::{self, Config};
use crate::plugin_tracker::PluginIndex;
use crate::plugins;
use crate::store::SecretStore;
use crate::suggest;

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
        bail!("{}", suggest::unknown_env(env, &config.environments));
    }

    // If the key isn't in config, offer to register it
    if config.find_secret(key).is_none() {
        let config_path = config.root.join("esk.yaml");
        if let Some(group) = group {
            config::add_secret_to_config(&config_path, key, group)?;
            cliclack::log::success(format!("Added '{}' to esk.yaml under {}", key, group))?;
        } else if atty::is(atty::Stream::Stdin) {
            let add = cliclack::confirm(format!("Secret '{}' is not in esk.yaml. Add it?", key))
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

    if secret_value.contains('\n') || secret_value.contains('\r') {
        cliclack::log::warning(
            "Secret contains newlines. Some adapters (fly, supabase) cannot sync newline-containing values.",
        )?;
    }

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
        plugin_failures =
            super::sync::push_to_plugins(&all_plugins, &payload, config, env, &mut plugin_index)?;
        plugin_index.save()?;

        if plugin_failures > 0 && strict {
            bail!(
                "{plugin_failures} plugin(s) failed to push (--strict). Adapter deploy skipped.\n\
                 Fix the plugin issue, then run:\n  \
                 esk sync --env {env}\n  \
                 esk deploy --env {env}"
            );
        }
    }

    // Auto-deploy affected targets
    crate::cli::deploy::run_with_runner(config, Some(env), false, false, false, runner)?;

    if plugin_failures > 0 {
        bail!("{plugin_failures} plugin(s) failed to push. Run `esk sync --env {env}` to retry.");
    }

    Ok(())
}
