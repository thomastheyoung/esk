use anyhow::Result;
use console::style;
use std::collections::{BTreeSet, HashMap};

use crate::adapters::{build_sync_adapters, CommandRunner, RealCommandRunner, SecretValue, SyncMode};
use crate::config::Config;
use crate::store::SecretStore;
use crate::tracker::SyncIndex;

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
    let index_path = config.root.join(".sync-index.json");
    let mut index = SyncIndex::load(&index_path)?;

    let resolved = config.resolve_secrets()?;

    let adapters = build_sync_adapters(config, runner);

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

    let mut sync_count = 0u32;
    let mut skip_count = 0u32;
    let mut fail_count = 0u32;

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
                        println!(
                            "  {} {}:{} — no value set",
                            style("skip").dim(),
                            secret.key,
                            target.environment
                        );
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
                        skip_count += 1;
                        if verbose {
                            println!(
                                "  {} {}:{} → {}",
                                style("skip").dim(),
                                secret.key,
                                target.environment,
                                target.adapter
                            );
                        }
                    }
                }
            }
        }
    }

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

        if dry_run {
            println!(
                "  {} {} ({} secrets) → {}",
                style("would sync").cyan(),
                style(adapter_name).bold(),
                secrets_for_batch.len(),
                target
            );
            sync_count += secrets_for_batch.len() as u32;
            continue;
        }

        if verbose {
            println!(
                "  {} {} ({} secrets) → {}",
                style("syncing").cyan(),
                style(adapter_name).bold(),
                secrets_for_batch.len(),
                target
            );
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
                sync_count += 1;
            } else {
                let error = result.error.clone().unwrap_or_default();
                index.record_failure(tracker_key, target.to_string(), value_hash, error.clone());
                fail_count += 1;
                eprintln!(
                    "  {} {}:{} → {}: {}",
                    style("fail").red(),
                    result.key,
                    target_env,
                    adapter_name,
                    error
                );
            }
        }
    }

    // Handle individual adapters
    for (key, value, target) in &individual_work {
        if dry_run {
            println!(
                "  {} {}:{} → {}",
                style("would sync").cyan(),
                key,
                target.environment,
                target
            );
            sync_count += 1;
            continue;
        }

        if verbose {
            println!(
                "  {} {}:{} → {}",
                style("syncing").cyan(),
                key,
                target.environment,
                target
            );
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
                sync_count += 1;
                println!(
                    "  {} {}:{} → {}",
                    style("synced").green(),
                    key,
                    target.environment,
                    target
                );
            }
            Err(e) => {
                index.record_failure(tracker_key, target.to_string(), value_hash, e.to_string());
                fail_count += 1;
                eprintln!(
                    "  {} {}:{} → {}: {}",
                    style("fail").red(),
                    key,
                    target.environment,
                    target,
                    e
                );
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
                        skip_count += 1;
                    }
                }
            }
        }
    }

    if !dry_run {
        index.save()?;
    }

    // Summary
    if dry_run {
        println!(
            "\n  {} {} to sync, {} up to date",
            style("dry run:").cyan(),
            sync_count,
            skip_count
        );
    } else {
        let mut parts = Vec::new();
        if sync_count > 0 {
            parts.push(format!("{} synced", sync_count));
        }
        if skip_count > 0 {
            parts.push(format!("{} up to date", skip_count));
        }
        if fail_count > 0 {
            parts.push(format!("{} failed", fail_count));
        }
        if parts.is_empty() {
            println!("  Nothing to sync.");
        } else {
            println!("\n  {}", parts.join(", "));
        }
    }

    if fail_count > 0 {
        anyhow::bail!("{fail_count} sync(s) failed");
    }

    Ok(())
}
