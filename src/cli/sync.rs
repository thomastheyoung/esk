use anyhow::Result;
use console::style;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::adapters::{build_sync_adapters, CommandRunner, RealCommandRunner, SecretValue, SyncMode};
use crate::config::Config;
use crate::store::SecretStore;
use crate::tracker::SyncIndex;

/// A single sync result entry for display.
struct SyncEntry {
    key: String,
    env: String,
    target: String,
    error: Option<String>,
}

pub fn run(
    config: &Config,
    env: Option<&str>,
    force: bool,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    run_with_runner(config, env, force, dry_run, verbose, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    env: Option<&str>,
    force: bool,
    dry_run: bool,
    verbose: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let index_path = config.root.join(".lockbox/sync-index.json");
    let mut index = SyncIndex::load(&index_path);

    let resolved = config.resolve_secrets()?;

    let has_configured_adapters = config.adapters.env.is_some()
        || config.adapters.cloudflare.is_some()
        || config.adapters.convex.is_some();

    let adapters = build_sync_adapters(config, runner);

    if adapters.is_empty() && has_configured_adapters {
        cliclack::log::warning("No adapters available after preflight checks. Fix the issues above and try again.")?;
        return Ok(());
    }

    // Build a lookup map: adapter_name -> (index, sync_mode)
    let adapter_map: HashMap<&str, (usize, SyncMode)> = adapters
        .iter()
        .enumerate()
        .map(|(i, a)| (a.name(), (i, a.sync_mode())))
        .collect();

    // Track batch-mode dirty target groups: (adapter_name, app, env)
    let mut batch_dirty: BTreeSet<(String, Option<String>, String)> = BTreeSet::new();
    // Individual-mode work items: (key, value, target)
    let mut individual_work: Vec<(String, String, crate::config::ResolvedTarget)> = Vec::new();

    // Collect structured results
    let mut synced: Vec<SyncEntry> = Vec::new();
    let mut skipped: Vec<SyncEntry> = Vec::new();
    let mut failed: Vec<SyncEntry> = Vec::new();

    for secret in &resolved {
        for target in &secret.targets {
            // Filter by environment if specified
            if let Some(filter_env) = env {
                if target.environment != filter_env {
                    continue;
                }
            }

            // Skip targets whose adapter isn't in the sync adapter map (e.g. plugins)
            let (_, sync_mode) = match adapter_map.get(target.adapter.as_str()) {
                Some(entry) => *entry,
                None => continue,
            };

            let composite = format!("{}:{}", secret.key, target.environment);
            let value = match payload.secrets.get(&composite) {
                Some(v) => v,
                None => {
                    if verbose {
                        cliclack::log::remark(format!(
                            "{}:{} — no value set",
                            secret.key, target.environment
                        ))?;
                    }
                    continue;
                }
            };

            let value_hash = SyncIndex::hash_value(value);
            let tracker_key = SyncIndex::tracker_key(
                &secret.key,
                &target.adapter,
                target.app.as_deref(),
                &target.environment,
            );

            match sync_mode {
                SyncMode::Batch => {
                    if index.should_sync(&tracker_key, &value_hash, force) {
                        batch_dirty.insert((
                            target.adapter.clone(),
                            target.app.clone(),
                            target.environment.clone(),
                        ));
                    }
                }
                SyncMode::Individual => {
                    if index.should_sync(&tracker_key, &value_hash, force) {
                        individual_work.push((secret.key.clone(), value.clone(), target.clone()));
                    } else {
                        skipped.push(SyncEntry {
                            key: secret.key.clone(),
                            env: target.environment.clone(),
                            target: format_target(target),
                            error: None,
                        });
                    }
                }
            }
        }
    }

    // Mark batch groups as dirty when tombstones exist for their secrets
    for composite_key in payload.tombstones.keys() {
        let Some((bare_key, tomb_env)) = composite_key.rsplit_once(':') else {
            continue;
        };
        if let Some(filter_env) = env {
            if tomb_env != filter_env {
                continue;
            }
        }
        for secret in &resolved {
            if secret.key != bare_key {
                continue;
            }
            for target in &secret.targets {
                if target.environment != tomb_env {
                    continue;
                }
                if let Some((_, SyncMode::Batch)) = adapter_map.get(target.adapter.as_str()) {
                    batch_dirty.insert((
                        target.adapter.clone(),
                        target.app.clone(),
                        target.environment.clone(),
                    ));
                }
            }
        }
    }

    // Normal mode: single spinner for the entire operation
    let spinner = if !verbose && !dry_run {
        let s = cliclack::spinner();
        s.start("Syncing secrets...");
        Some(s)
    } else {
        None
    };

    // Handle batch adapters: for each dirty target group, gather ALL secrets and sync
    for (adapter_name, app, target_env) in &batch_dirty {
        let (adapter_idx, _) = adapter_map[adapter_name.as_str()];
        let adapter = &adapters[adapter_idx];

        // Gather all secrets that target this (adapter, app, env)
        let mut secrets_for_batch: Vec<SecretValue> = Vec::new();
        for secret in &resolved {
            for target in &secret.targets {
                if target.adapter == *adapter_name
                    && target.app.as_ref() == app.as_ref()
                    && target.environment == *target_env
                {
                    let composite = format!("{}:{}", secret.key, target_env);
                    if let Some(value) = payload.secrets.get(&composite) {
                        secrets_for_batch.push(SecretValue {
                            key: secret.key.clone(),
                            value: value.clone(),
                            vendor: secret.vendor.clone(),
                        });
                    }
                }
            }
        }

        if secrets_for_batch.is_empty() {
            continue;
        }

        let target = crate::config::ResolvedTarget {
            adapter: adapter_name.clone(),
            app: app.clone(),
            environment: target_env.clone(),
        };

        let target_display = format_target(&target);

        if dry_run {
            for s in &secrets_for_batch {
                synced.push(SyncEntry {
                    key: s.key.clone(),
                    env: target_env.clone(),
                    target: target_display.clone(),
                    error: None,
                });
            }
            continue;
        }

        if verbose {
            cliclack::log::step(format!(
                "Syncing {} ({} secrets) → {}",
                style(adapter_name).bold(),
                secrets_for_batch.len(),
                target
            ))?;
        }

        let results = adapter.sync_batch(&secrets_for_batch, &target);

        for result in &results {
            let tracker_key =
                SyncIndex::tracker_key(&result.key, adapter_name, app.as_deref(), target_env);
            let composite = format!("{}:{}", result.key, target_env);
            let value = payload
                .secrets
                .get(&composite)
                .map(|v| v.as_str())
                .unwrap_or("");
            let value_hash = SyncIndex::hash_value(value);

            if result.success {
                index.record_success(tracker_key, target.to_string(), value_hash);
                synced.push(SyncEntry {
                    key: result.key.clone(),
                    env: target_env.clone(),
                    target: target_display.clone(),
                    error: None,
                });
            } else {
                let error = result.error.clone().unwrap_or_default();
                index.record_failure(tracker_key, target.to_string(), value_hash, error.clone());
                failed.push(SyncEntry {
                    key: result.key.clone(),
                    env: target_env.clone(),
                    target: target_display.clone(),
                    error: Some(error),
                });
            }
        }

        // Save index after each batch group
        if !dry_run {
            index.save()?;
        }
    }

    // Handle individual adapters
    for (key, value, target) in &individual_work {
        let target_display = format_target(target);

        if dry_run {
            synced.push(SyncEntry {
                key: key.clone(),
                env: target.environment.clone(),
                target: target_display,
                error: None,
            });
            continue;
        }

        if verbose {
            cliclack::log::step(format!(
                "Syncing {}:{} → {}",
                key, target.environment, target
            ))?;
        }

        let (adapter_idx, _) = adapter_map[target.adapter.as_str()];
        let adapter = &adapters[adapter_idx];
        let result = adapter.sync_secret(key, value, target);

        let tracker_key = SyncIndex::tracker_key(
            key,
            &target.adapter,
            target.app.as_deref(),
            &target.environment,
        );
        let value_hash = SyncIndex::hash_value(value);

        match result {
            Ok(()) => {
                index.record_success(tracker_key, target.to_string(), value_hash);
                synced.push(SyncEntry {
                    key: key.clone(),
                    env: target.environment.clone(),
                    target: target_display,
                    error: None,
                });
                if verbose {
                    cliclack::log::success(format!(
                        "Synced {}:{} → {}",
                        key, target.environment, target
                    ))?;
                }
            }
            Err(e) => {
                index.record_failure(tracker_key, target.to_string(), value_hash, e.to_string());
                failed.push(SyncEntry {
                    key: key.clone(),
                    env: target.environment.clone(),
                    target: target_display,
                    error: Some(e.to_string()),
                });
                if verbose {
                    let _ = cliclack::log::error(format!(
                        "{}:{} → {}: {}",
                        key, target.environment, target, e
                    ));
                }
            }
        }

        // Save index after each individual secret
        index.save()?;
    }

    // Process tombstones: delete secrets from individual adapters
    for composite_key in payload.tombstones.keys() {
        // Parse composite key "KEY:env"
        let Some((bare_key, tomb_env)) = composite_key.rsplit_once(':') else {
            continue;
        };

        // Filter by environment if specified
        if let Some(filter_env) = env {
            if tomb_env != filter_env {
                continue;
            }
        }

        // Find targets for this key in the resolved secrets list
        for secret in &resolved {
            if secret.key != bare_key {
                continue;
            }
            for target in &secret.targets {
                if target.environment != tomb_env {
                    continue;
                }
                let Some((_, SyncMode::Individual)) = adapter_map.get(target.adapter.as_str())
                else {
                    continue;
                };
                let (adapter_idx, _) = adapter_map[target.adapter.as_str()];
                let adapter = &adapters[adapter_idx];

                if dry_run {
                    continue;
                }

                if let Err(e) = adapter.delete_secret(bare_key, target) {
                    if verbose {
                        let _ = cliclack::log::warning(format!(
                            "Failed to delete {}:{} from {}: {}",
                            bare_key,
                            tomb_env,
                            format_target(target),
                            e
                        ));
                    }
                }
            }
        }
    }

    // Count skipped batch secrets (those in non-dirty batch target groups)
    for secret in &resolved {
        for target in &secret.targets {
            if let Some((_, SyncMode::Batch)) = adapter_map.get(target.adapter.as_str()) {
                if let Some(filter_env) = env {
                    if target.environment != filter_env {
                        continue;
                    }
                }
                let group = (
                    target.adapter.clone(),
                    target.app.clone(),
                    target.environment.clone(),
                );
                if !batch_dirty.contains(&group) {
                    let composite = format!("{}:{}", secret.key, target.environment);
                    if payload.secrets.contains_key(&composite) {
                        skipped.push(SyncEntry {
                            key: secret.key.clone(),
                            env: target.environment.clone(),
                            target: format_target(target),
                            error: None,
                        });
                    }
                }
            }
        }
    }

    if !dry_run {
        index.save()?;
    }

    let sync_count = synced.len();
    let skip_count = skipped.len();
    let fail_count = failed.len();

    // Stop spinner before printing results
    if let Some(s) = spinner {
        s.stop("Syncing secrets...");
    }

    // Output
    if dry_run {
        print_group("synced", &synced)?;
        if skip_count > 0 {
            cliclack::log::remark(format!(
                "{} up to date",
                style(skip_count).bold()
            ))?;
        }
        cliclack::log::warning(format!("Dry run — no changes made"))?;
    } else if sync_count == 0 && skip_count == 0 && fail_count == 0 {
        cliclack::log::info("Nothing to sync.")?;
    } else if fail_count == 0 && sync_count == 0 && skip_count > 0 {
        // Everything up to date
        cliclack::log::success(format!(
            "All {} targets up to date",
            style(skip_count).bold()
        ))?;
    } else {
        // Print failed first (most important)
        print_group("failed", &failed)?;

        // Print synced
        print_group("synced", &synced)?;

        // Print skipped summary
        if skip_count > 0 {
            if verbose {
                print_group("up to date", &skipped)?;
            } else {
                cliclack::log::remark(format!(
                    "{} up to date  {}",
                    style(skip_count).bold(),
                    style("(use --verbose to show)").dim()
                ))?;
            }
        }
    }

    if fail_count > 0 {
        anyhow::bail!("{fail_count} sync(s) failed");
    }

    Ok(())
}

/// Group entries by (key, env) → [targets], collapsing same key+env.
/// Returns Vec<(key, env, targets, error)>.
fn group_entries(entries: &[SyncEntry]) -> Vec<(&str, &str, Vec<&str>, Option<&str>)> {
    let mut map: BTreeMap<(&str, &str), (Vec<&str>, Option<&str>)> = BTreeMap::new();
    for entry in entries {
        let group = map
            .entry((&entry.key, &entry.env))
            .or_insert_with(|| (Vec::new(), None));
        group.0.push(&entry.target);
        if entry.error.is_some() {
            group.1 = entry.error.as_deref();
        }
    }
    map.into_iter()
        .map(|((key, env), (targets, error))| (key, env, targets, error))
        .collect()
}

fn print_group(label: &str, entries: &[SyncEntry]) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }

    let grouped = group_entries(entries);

    let (icon, count_style) = match label {
        "failed" => ("✗", style(grouped.len()).red().bold()),
        "synced" => ("✓", style(grouped.len()).green().bold()),
        _ => ("●", style(grouped.len()).dim().bold()),
    };

    let header = format!("{icon} {count_style} {label}");

    let lines: Vec<String> = grouped
        .iter()
        .map(|(key, env, targets, error)| {
            let targets_str = targets.join(", ");
            let mut line = format!(
                "  {}  → {}",
                style(format!("{key}:{env}")).dim(),
                targets_str
            );
            if let Some(err) = error {
                line.push_str(&format!("  {}", style(format!("({err})")).dim()));
            }
            line
        })
        .collect();

    let body = format!("{header}\n{}", lines.join("\n"));

    match label {
        "failed" => cliclack::log::error(body)?,
        "synced" => cliclack::log::success(body)?,
        _ => cliclack::log::remark(body)?,
    }

    Ok(())
}

fn format_target(target: &crate::config::ResolvedTarget) -> String {
    let mut s = target.adapter.clone();
    if let Some(app) = &target.app {
        s.push(':');
        s.push_str(app);
    }
    s
}
