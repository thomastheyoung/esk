use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;
use std::io::IsTerminal;

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::Config;
use crate::plugin_tracker::PluginIndex;
use crate::plugins::{self, StoragePlugin};
use crate::reconcile::{self, ConflictPreference};
use crate::store::{SecretStore, StorePayload};
use crate::suggest;
use crate::ui;

pub struct SyncOptions<'a> {
    pub env: Option<&'a str>,
    pub only: Option<&'a str>,
    pub dry_run: bool,
    pub no_partial: bool,
    pub force: bool,
    pub auto_deploy: bool,
    pub prefer: ConflictPreference,
}

fn env_version_label(payload: &StorePayload, env: &str) -> String {
    let version = payload.env_version(env);
    match payload.env_last_changed_at(env) {
        Some(ts) => format!("v{} ({})", version, ui::format_relative_time(ts)),
        None => format!("v{}", version),
    }
}

fn visible_width(text: &str) -> usize {
    console::strip_ansi_codes(text).chars().count()
}

fn format_sync_line(label: &str, value: &str, value_column: usize) -> String {
    let label_width = visible_width(label);
    let dots = ".".repeat(value_column.saturating_sub(label_width).max(3));
    format!("{} {} {}", label, style(dots).dim(), value)
}

/// Push a payload to the given plugins, recording results in the plugin index.
/// Returns the number of failures.
pub fn push_to_plugins(
    plugins: &[Box<dyn StoragePlugin + '_>],
    payload: &StorePayload,
    config: &Config,
    env: &str,
    plugin_index: &mut PluginIndex,
) -> Result<u32> {
    let mut fail_count = 0u32;
    let pushed_version = payload.env_version(env);

    for plugin in plugins {
        let spinner = cliclack::spinner();
        spinner.start(format!("↑ {}...", plugin.name()));

        match plugin.push(payload, config, env) {
            Ok(()) => {
                spinner.stop(format!("↑ {}  {}", plugin.name(), style("done").green()));
                plugin_index.record_success(plugin.name(), env, pushed_version);
            }
            Err(e) => {
                spinner.error(format!(
                    "↑ {}  {} — {e}",
                    plugin.name(),
                    style("failed").red()
                ));
                plugin_index.record_failure(plugin.name(), env, pushed_version, e.to_string());
                fail_count += 1;
            }
        }
    }

    Ok(fail_count)
}

pub fn run(config: &Config, options: SyncOptions<'_>) -> Result<()> {
    let runner = RealCommandRunner;
    let envs: Vec<&str> = match options.env {
        Some(e) => {
            if !config.environments.contains(&e.to_string()) {
                bail!("{}", suggest::unknown_env(e, &config.environments));
            }
            vec![e]
        }
        None => config.environments.iter().map(|s| s.as_str()).collect(),
    };

    if envs.len() == 1 {
        return run_with_runner(
            config,
            envs[0],
            options.only,
            options.dry_run,
            options.no_partial,
            options.force,
            options.auto_deploy,
            options.prefer,
            &runner,
        );
    }

    let mut failures: Vec<String> = Vec::new();
    for env in &envs {
        if let Err(e) = run_with_runner(
            config,
            env,
            options.only,
            options.dry_run,
            options.no_partial,
            options.force,
            options.auto_deploy,
            options.prefer,
            &runner,
        ) {
            if options.no_partial {
                bail!("sync failed for environment '{env}': {e}");
            }
            cliclack::log::error(format!("sync failed for environment '{env}': {e}"))?;
            failures.push(env.to_string());
        }
    }

    if !failures.is_empty() {
        bail!(
            "{} environment(s) failed to sync: {}",
            failures.len(),
            failures.join(", ")
        );
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_runner(
    config: &Config,
    env: &str,
    only: Option<&str>,
    dry_run: bool,
    no_partial: bool,
    force: bool,
    auto_deploy: bool,
    prefer: ConflictPreference,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!("{}", suggest::unknown_env(env, &config.environments));
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
        let plugin_names: Vec<String> = all_plugins.iter().map(|p| p.name().to_string()).collect();
        let filtered: Vec<_> = all_plugins
            .into_iter()
            .filter(|p| p.name() == name)
            .collect();
        if filtered.is_empty() {
            bail!("{}", suggest::unknown_plugin(name, &plugin_names));
        }
        filtered
    } else {
        all_plugins
    };

    let mut lines = Vec::new();
    let value_column = 24;

    // Pull from all target plugins
    let mut remote_data: Vec<(String, BTreeMap<String, String>, u64)> = Vec::new();
    let mut pull_failures: Vec<String> = Vec::new();

    for plugin in &target_plugins {
        match plugin.pull(config, env) {
            Ok(Some((secrets, version))) => {
                lines.push(format_sync_line(
                    &format!("↓ {}", plugin.name()),
                    &format!("v{}, {} secrets", version, secrets.len()),
                    value_column,
                ));
                remote_data.push((plugin.name().to_string(), secrets, version));
            }
            Ok(None) => {
                lines.push(format_sync_line(
                    &format!("↓ {}", plugin.name()),
                    &style("no data").dim().to_string(),
                    value_column,
                ));
            }
            Err(_) => {
                lines.push(format_sync_line(
                    &format!("↓ {}", plugin.name()),
                    &style("failed").red().to_string(),
                    value_column,
                ));
                pull_failures.push(plugin.name().to_string());
            }
        }
    }

    if !pull_failures.is_empty() {
        if no_partial {
            bail!(
                "{} plugin(s) failed to respond: {}. Use without --no-partial to reconcile with partial data.",
                pull_failures.len(),
                pull_failures.join(", ")
            );
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

    let result = match reconcile::reconcile_multi_with_jump_limit(
        &payload,
        &remotes,
        Some(env),
        prefer,
        !force,
    ) {
        Ok(r) => r,
        Err(e) if reconcile::is_version_jump_error(&e) && !force => {
            if std::io::stdin().is_terminal() {
                cliclack::log::warning(format!("{e}"))?;
                let accept = cliclack::confirm(
                    "Accept the large version jump? This may indicate a compromised plugin.",
                )
                .initial_value(false)
                .interact()?;
                if !accept {
                    bail!("Aborted by user. Use --force to bypass version jump protection.");
                }
            } else {
                bail!("{e}\nRun with --force to bypass version jump protection.");
            }
            reconcile::reconcile_multi_with_jump_limit(
                &payload,
                &remotes,
                Some(env),
                prefer,
                false,
            )?
        }
        Err(e) => return Err(e),
    };

    let mut status_line = String::new();

    // Dry-run exit point
    if dry_run {
        if result.local_changed {
            let label = env_version_label(&result.merged_payload, env);
            status_line = format!("{} Would merge → {}", style("↻").yellow(), label);
        } else if result.sources_to_update.is_empty() {
            let label = env_version_label(&payload, env);
            status_line = format!("{} Up to date → {}", style("✔").green(), label);
        }

        if !result.sources_to_update.is_empty() {
            for name in &result.sources_to_update {
                lines.push(format_sync_line(
                    &format!("↑ {name}"),
                    &style("would push").dim().to_string(),
                    value_column,
                ));
            }
            if result.has_drift {
                let label = env_version_label(&payload, env);
                status_line = format!(
                    "{} Current ({}), would repair drift",
                    style("↻").yellow(),
                    label
                );
            }
        }
        lines.push(String::new());
        lines.push(status_line);
        cliclack::note(env, lines.join("\n"))?;
        return Ok(());
    }

    if result.local_changed {
        store.set_payload(&result.merged_payload)?;
        let label = env_version_label(&result.merged_payload, env);
        status_line = format!("{} Merged → {}", style("↻").yellow(), label);
    } else {
        let label = env_version_label(&payload, env);
        if result.has_drift {
            status_line = format!("{} Stale plugins (repairing...)", style("↻").yellow());
        } else {
            status_line = format!("{} Up to date → {}", style("✔").green(), label);
        }
    }

    // Push merged/current result back to stale plugins (no interactive prompt)
    if !result.sources_to_update.is_empty() {
        let updated_payload = if result.local_changed {
            &result.merged_payload
        } else {
            &payload
        };
        let plugin_index_path = config.root.join(".esk/plugin-index.json");
        let mut plugin_index = PluginIndex::load(&plugin_index_path);

        let stale_plugins: Vec<_> = target_plugins
            .iter()
            .filter(|p| result.sources_to_update.contains(&p.name().to_string()))
            .collect();
        let pushed_version = updated_payload.env_version(env);

        let mut pushback_failures = 0u32;
        for plugin in &stale_plugins {
            match plugin.push(updated_payload, config, env) {
                Ok(()) => {
                    lines.push(format_sync_line(
                        &format!("↑ {}", plugin.name()),
                        &style("synced").green().to_string(),
                        value_column,
                    ));
                    plugin_index.record_success(plugin.name(), env, pushed_version);
                }
                Err(e) => {
                    lines.push(format_sync_line(
                        &format!("↑ {}", plugin.name()),
                        &style("failed").red().to_string(),
                        value_column,
                    ));
                    plugin_index.record_failure(plugin.name(), env, pushed_version, e.to_string());
                    pushback_failures += 1;
                }
            }
        }
        plugin_index.save()?;
        if pushback_failures > 0 {
            lines.push(String::new());
            lines.push(status_line);
            cliclack::note(env, lines.join("\n"))?;
            bail!(
                "{pushback_failures} plugin(s) failed to receive merged data. Run `esk sync --env {env}` to retry."
            );
        }
    }

    lines.push(String::new());
    lines.push(status_line);
    cliclack::note(env, lines.join("\n"))?;

    if auto_deploy && result.local_changed {
        cliclack::log::step("Running deploy...")?;
        crate::cli::deploy::run_with_runner(config, Some(env), false, false, false, runner)?;
    }

    Ok(())
}
