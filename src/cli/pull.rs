use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::Config;
use crate::plugin_tracker::PluginIndex;
use crate::plugins;
use crate::reconcile;
use crate::store::SecretStore;

pub fn run(
    config: &Config,
    env: &str,
    only: Option<&str>,
    auto_sync: bool,
    strict: bool,
) -> Result<()> {
    run_with_runner(config, env, only, auto_sync, strict, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    env: &str,
    only: Option<&str>,
    auto_sync: bool,
    strict: bool,
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
            cliclack::log::warning("No plugins available after preflight checks. Fix the issues above and try again.")?;
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

    // Pull from all target plugins
    let mut remote_data: Vec<(String, BTreeMap<String, String>, u64)> = Vec::new();
    let mut pull_failures: Vec<String> = Vec::new();

    for plugin in &target_plugins {
        let spinner = cliclack::spinner();
        spinner.start(format!("Pulling ← {}...", plugin.name()));

        match plugin.pull(config, env) {
            Ok(Some((secrets, version))) => {
                spinner.stop(format!(
                    "Pulled ← {} {} (v{}, {} secrets)",
                    plugin.name(),
                    style("ok").green(),
                    version,
                    secrets.len()
                ));
                remote_data.push((plugin.name().to_string(), secrets, version));
            }
            Ok(None) => {
                spinner.stop(format!(
                    "Pulled ← {} {}",
                    plugin.name(),
                    style("no data").dim()
                ));
            }
            Err(e) => {
                spinner.error(format!(
                    "Pulled ← {} {} — {e}",
                    plugin.name(),
                    style("failed").red()
                ));
                pull_failures.push(plugin.name().to_string());
            }
        }
    }

    if !pull_failures.is_empty() {
        if strict {
            bail!(
                "{} plugin(s) failed to respond: {}. Use without --strict to reconcile with partial data.",
                pull_failures.len(),
                pull_failures.join(", ")
            );
        }
        if !remote_data.is_empty() {
            cliclack::log::warning(format!(
                "{} plugin(s) failed to respond. Reconciliation used partial data.",
                pull_failures.len()
            ))?;
        }
    }

    if remote_data.is_empty() {
        cliclack::log::info("No remote data found. Nothing to reconcile.")?;
        return Ok(());
    }

    // Multi-source reconciliation
    let remotes: Vec<(&str, &BTreeMap<String, String>, u64)> = remote_data
        .iter()
        .map(|(name, secrets, version)| (name.as_str(), secrets, *version))
        .collect();

    let result = reconcile::reconcile_multi(&payload, &remotes, Some(env));

    if result.local_changed {
        store.set_payload(&result.merged_payload)?;
        cliclack::log::success(format!(
            "Merged — local store updated to v{}",
            result.merged_payload.version
        ))?;

        // Push merged result back to plugins that were behind
        if !result.sources_to_update.is_empty() {
            let updated_payload = store.payload()?;
            let plugin_index_path = config.root.join(".lockbox/plugin-index.json");
            let mut plugin_index = PluginIndex::load(&plugin_index_path);
            let mut pushback_failures = 0u32;
            for plugin in &target_plugins {
                if result
                    .sources_to_update
                    .contains(&plugin.name().to_string())
                {
                    let spinner = cliclack::spinner();
                    spinner.start(format!("Pushing merged → {}...", plugin.name()));
                    match plugin.push(&updated_payload, config, env) {
                        Ok(()) => {
                            spinner.stop(format!(
                                "Pushed merged → {} {}",
                                plugin.name(),
                                style("done").green()
                            ));
                            plugin_index.record_success(
                                plugin.name(),
                                env,
                                updated_payload.version,
                            );
                        }
                        Err(e) => {
                            spinner.error(format!(
                                "Pushed merged → {} {} — {e}",
                                plugin.name(),
                                style("failed").red()
                            ));
                            plugin_index.record_failure(
                                plugin.name(),
                                env,
                                updated_payload.version,
                                e.to_string(),
                            );
                            pushback_failures += 1;
                        }
                    }
                }
            }
            plugin_index.save()?;
            if pushback_failures > 0 {
                bail!(
                    "{pushback_failures} plugin(s) failed to receive merged data. Run `lockbox push --env {env}` to retry."
                );
            }
        }
    } else {
        cliclack::log::success(format!(
            "Up to date — already in sync (v{})",
            payload.version
        ))?;
    }

    if auto_sync && result.local_changed {
        cliclack::log::step("Running sync...")?;
        crate::cli::sync::run_with_runner(config, Some(env), false, false, false, runner)?;
    }

    Ok(())
}
