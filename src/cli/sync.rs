use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;
use std::io::IsTerminal;

use crate::config::Config;
use crate::reconcile::{self, ConflictPreference};
use crate::remotes::{self, SyncRemote};
use crate::store::{SecretStore, StorePayload};
use crate::suggest;
use crate::sync_tracker::SyncIndex;
use crate::targets::{CommandRunner, RealCommandRunner};
use crate::ui;

#[derive(Clone, Copy)]
pub struct SyncOptions<'a> {
    pub env: Option<&'a str>,
    pub only: Option<&'a str>,
    pub dry_run: bool,
    pub bail: bool,
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

/// Push a payload to the given remotes, recording results in the remote index.
/// Returns the number of failures.
pub fn push_to_remotes(
    remotes: &[Box<dyn SyncRemote + '_>],
    payload: &StorePayload,
    config: &Config,
    env: &str,
    sync_index: &mut SyncIndex,
) -> Result<u32> {
    let mut fail_count = 0u32;
    let pushed_version = payload.env_version(env);

    for rem in remotes {
        let spinner = cliclack::spinner();
        spinner.start(format!("↑ {}...", rem.name()));

        match rem.push(payload, config, env) {
            Ok(()) => {
                spinner.stop(format!("↑ {}  {}", rem.name(), style("done").green()));
                sync_index.record_success(rem.name(), env, pushed_version);
            }
            Err(e) => {
                spinner.error(format!("↑ {}  {} — {e}", rem.name(), style("failed").red()));
                sync_index.record_failure(rem.name(), env, pushed_version, e.to_string());
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
            config.validate_env(e)?;
            vec![e]
        }
        None => config.environments.iter().map(|s| s.as_str()).collect(),
    };

    if envs.len() == 1 {
        return run_with_runner(config, &options, &runner);
    }

    let mut failures: Vec<String> = Vec::new();
    for env in &envs {
        let per_env_opts = SyncOptions {
            env: Some(env),
            ..options
        };
        if let Err(e) = run_with_runner(config, &per_env_opts, &runner) {
            if options.bail {
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

pub fn run_with_runner(
    config: &Config,
    opts: &SyncOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let env = opts.env.expect("sync requires an environment");
    config.validate_env(env)?;
    let only = opts.only;
    let dry_run = opts.dry_run;
    let bail = opts.bail;
    let force = opts.force;
    let auto_deploy = opts.auto_deploy;
    let prefer = opts.prefer;

    if config.remotes.is_empty() {
        bail!("no remotes configured in esk.yaml");
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;

    let all_remotes = remotes::build_remotes(config, runner);

    if all_remotes.is_empty() {
        if config.remotes.is_empty() {
            bail!("no remotes configured in esk.yaml");
        } else {
            cliclack::log::warning(
                "No remotes available after preflight checks. Fix the issues above and try again.",
            )?;
            return Ok(());
        }
    }

    // Filter by --only if provided
    let target_remotes: Vec<_> = if let Some(name) = only {
        let remote_names: Vec<String> = all_remotes.iter().map(|p| p.name().to_string()).collect();
        let filtered: Vec<_> = all_remotes
            .into_iter()
            .filter(|p| p.name() == name)
            .collect();
        if filtered.is_empty() {
            bail!("{}", suggest::unknown_remote(name, &remote_names));
        }
        filtered
    } else {
        all_remotes
    };

    let mut lines = Vec::new();
    let value_column = 24;

    // Pull from all target remotes
    let mut remote_data: Vec<(String, BTreeMap<String, String>, u64)> = Vec::new();
    let mut pull_failures: Vec<String> = Vec::new();

    for rem in &target_remotes {
        match rem.pull(config, env) {
            Ok(Some((secrets, version))) => {
                lines.push(format_sync_line(
                    &format!("↓ {}", rem.name()),
                    &format!("v{}, {} secrets", version, secrets.len()),
                    value_column,
                ));
                remote_data.push((rem.name().to_string(), secrets, version));
            }
            Ok(None) => {
                lines.push(format_sync_line(
                    &format!("↓ {}", rem.name()),
                    &style("no data").dim().to_string(),
                    value_column,
                ));
            }
            Err(_) => {
                lines.push(format_sync_line(
                    &format!("↓ {}", rem.name()),
                    &style("failed").red().to_string(),
                    value_column,
                ));
                pull_failures.push(rem.name().to_string());
            }
        }
    }

    if !pull_failures.is_empty() && bail {
        bail!(
            "{} remote(s) failed to respond: {}. Use without --bail to reconcile with partial data.",
            pull_failures.len(),
            pull_failures.join(", ")
        );
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
                    "Accept the large version jump? This may indicate a compromised remote.",
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
            status_line = format!("{} Stale remotes (repairing...)", style("↻").yellow());
        } else {
            status_line = format!("{} Up to date → {}", style("✔").green(), label);
        }
    }

    // Push merged/current result back to stale remotes (no interactive prompt)
    if !result.sources_to_update.is_empty() {
        let updated_payload = if result.local_changed {
            &result.merged_payload
        } else {
            &payload
        };
        let sync_index_path = config.root.join(".esk/sync-index.json");
        let mut sync_index = SyncIndex::load(&sync_index_path);

        let stale_remotes: Vec<_> = target_remotes
            .iter()
            .filter(|p| result.sources_to_update.contains(&p.name().to_string()))
            .collect();
        let pushed_version = updated_payload.env_version(env);

        let mut pushback_failures = 0u32;
        for rem in &stale_remotes {
            match rem.push(updated_payload, config, env) {
                Ok(()) => {
                    lines.push(format_sync_line(
                        &format!("↑ {}", rem.name()),
                        &style("synced").green().to_string(),
                        value_column,
                    ));
                    sync_index.record_success(rem.name(), env, pushed_version);
                }
                Err(e) => {
                    lines.push(format_sync_line(
                        &format!("↑ {}", rem.name()),
                        &style("failed").red().to_string(),
                        value_column,
                    ));
                    sync_index.record_failure(rem.name(), env, pushed_version, e.to_string());
                    pushback_failures += 1;
                }
            }
        }
        sync_index.save()?;
        if pushback_failures > 0 {
            lines.push(String::new());
            lines.push(status_line);
            cliclack::note(env, lines.join("\n"))?;
            bail!(
                "{pushback_failures} remote(s) failed to receive merged data. Run `esk sync --env {env}` to retry."
            );
        }
    }

    lines.push(String::new());
    lines.push(status_line);
    cliclack::note(env, lines.join("\n"))?;

    if auto_deploy && result.local_changed {
        cliclack::log::step("Running deploy...")?;
        crate::cli::deploy::run_with_runner(
            config,
            &crate::cli::deploy::DeployOptions {
                env: Some(env),
                force: false,
                dry_run: false,
                verbose: false,
                skip_validation: false,
                skip_requirements: true,
            },
            runner,
        )?;
    }

    Ok(())
}
