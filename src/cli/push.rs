use anyhow::{bail, Result};
use console::style;

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::Config;
use crate::plugin_tracker::PluginIndex;
use crate::plugins;
use crate::store::SecretStore;

pub fn run(config: &Config, env: &str, only: Option<&str>) -> Result<()> {
    run_with_runner(config, env, only, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    env: &str,
    only: Option<&str>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!(
            "unknown environment '{env}'. Valid: {}",
            config.environments.join(", ")
        );
    }

    if config.plugins.is_empty() {
        bail!("no plugins configured in lockbox.yaml");
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;

    let all_plugins = plugins::build_plugins(config, runner);

    if all_plugins.is_empty() {
        if config.plugins.is_empty() {
            bail!("no plugins configured in lockbox.yaml");
        } else {
            cliclack::log::warning(
                "No plugins available after preflight checks. Fix the issues above and try again.",
            )?;
            return Ok(());
        }
    }

    // Filter by --only if provided
    let target_plugins: Vec<_> = if let Some(name) = only {
        let filtered: Vec<_> = all_plugins
            .into_iter()
            .filter(|p| p.name() == name)
            .collect();
        if filtered.is_empty() {
            bail!("unknown plugin '{name}'");
        }
        filtered
    } else {
        all_plugins
    };

    let plugin_index_path = config.root.join(".lockbox/plugin-index.json");
    let mut plugin_index = PluginIndex::load(&plugin_index_path);

    let mut success_count = 0u32;
    let mut fail_count = 0u32;

    for plugin in &target_plugins {
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
                success_count += 1;
            }
            Err(e) => {
                spinner.error(format!(
                    "Pushed → {} {} — {e}",
                    plugin.name(),
                    style("failed").red()
                ));
                plugin_index.record_failure(plugin.name(), env, payload.version, e.to_string());
                fail_count += 1;
            }
        }
    }

    plugin_index.save()?;

    if fail_count == 0 {
        cliclack::log::success(format!(
            "{} plugin(s) pushed (v{})",
            success_count, payload.version
        ))?;
    } else {
        cliclack::log::error(format!(
            "{} pushed, {} failed (v{})",
            success_count, fail_count, payload.version
        ))?;
    }

    if fail_count > 0 {
        bail!("{fail_count} plugin push(es) failed");
    }

    Ok(())
}
