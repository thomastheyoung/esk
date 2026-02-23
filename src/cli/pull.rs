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
    force: bool,
) -> Result<()> {
    run_with_runner(config, env, only, auto_sync, strict, force, &RealCommandRunner)
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_runner(
    config: &Config,
    env: &str,
    only: Option<&str>,
    auto_sync: bool,
    strict: bool,
    force: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!(
            "unknown environment '{env}'. Valid: {}",
            config.environments.join(", ")
        );
    }

    if config.plugins.is_empty() {
        bail!("no plugins configured in esk.yaml");
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;

    let all_plugins = plugins::build_plugins(config, runner);

    if all_plugins.is_empty() {
        if config.plugins.is_empty() {
            bail!("no plugins configured in esk.yaml");
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

    let result = match reconcile::reconcile_multi(&payload, &remotes, Some(env)) {
        Ok(r) => r,
        Err(e) if e.to_string().contains("version jump too large") && !force => {
            if atty::is(atty::Stream::Stdin) {
                cliclack::log::warning(format!("{e}"))?;
                let accept = cliclack::confirm(
                    "Accept the large version jump? This may indicate a compromised plugin.",
                )
                .initial_value(false)
                .interact()?;
                if !accept {
                    bail!("Aborted by user. Use --force to bypass version jump protection.");
                }
                // Retry with a very high limit by directly calling without the check
                // We re-call and the error won't happen since we handle it here
                // Actually, we need to bypass. Let's use a simpler approach:
                // just proceed by not propagating the error.
                // Since we can't easily bypass, we re-do with force semantics.
                // The simplest approach: catch the error and note the user accepted.
            } else {
                bail!(
                    "{e}\nRun with --force to bypass version jump protection."
                );
            }
            // If we get here, user confirmed interactively. We need to recompute.
            // Use a local payload with inflated version to bypass the check.
            let mut adjusted_payload = payload.clone();
            // Set local version high enough to bypass the jump check
            let max_remote = remotes.iter().map(|(_, _, v)| *v).max().unwrap_or(0);
            if let Some(e_str) = adjusted_payload.env_versions.get_mut(env) {
                *e_str = max_remote.saturating_sub(reconcile::MAX_VERSION_JUMP);
            } else {
                adjusted_payload.env_versions.insert(
                    env.to_string(),
                    max_remote.saturating_sub(reconcile::MAX_VERSION_JUMP),
                );
            }
            reconcile::reconcile_multi(&adjusted_payload, &remotes, Some(env))?
        }
        Err(e) if e.to_string().contains("version jump too large") && force => {
            // --force: adjust local version to bypass the check
            let mut adjusted_payload = payload.clone();
            let max_remote = remotes.iter().map(|(_, _, v)| *v).max().unwrap_or(0);
            if let Some(e_str) = adjusted_payload.env_versions.get_mut(env) {
                *e_str = max_remote.saturating_sub(reconcile::MAX_VERSION_JUMP);
            } else {
                adjusted_payload.env_versions.insert(
                    env.to_string(),
                    max_remote.saturating_sub(reconcile::MAX_VERSION_JUMP),
                );
            }
            reconcile::reconcile_multi(&adjusted_payload, &remotes, Some(env))?
        }
        Err(e) => return Err(e),
    };

    if result.local_changed {
        store.set_payload(&result.merged_payload)?;
        cliclack::log::success(format!(
            "Merged — local store updated to v{}",
            result.merged_payload.version
        ))?;

        // Push merged result back to plugins that were behind
        let should_pushback = if result.sources_to_update.is_empty() {
            false
        } else if !atty::is(atty::Stream::Stdin) {
            cliclack::log::warning(
                "Skipped auto-pushback in non-interactive mode. Run `esk push` to update stale plugins.",
            )?;
            false
        } else {
            let plugin_names = result.sources_to_update.join(", ");
            cliclack::confirm(format!(
                "Push merged result back to {} plugin(s)? ({})",
                result.sources_to_update.len(),
                plugin_names
            ))
            .initial_value(true)
            .interact()?
        };
        if should_pushback {
            let updated_payload = &result.merged_payload;
            let plugin_index_path = config.root.join(".esk/plugin-index.json");
            let mut plugin_index = PluginIndex::load(&plugin_index_path);
            let mut pushback_failures = 0u32;
            for plugin in &target_plugins {
                if result
                    .sources_to_update
                    .contains(&plugin.name().to_string())
                {
                    let spinner = cliclack::spinner();
                    spinner.start(format!("Pushing merged → {}...", plugin.name()));
                    match plugin.push(updated_payload, config, env) {
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
                    "{pushback_failures} plugin(s) failed to receive merged data. Run `esk push --env {env}` to retry."
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
