use anyhow::{Context, Result};
use console::style;
use std::collections::BTreeSet;

use crate::adapters::env_file::EnvFileAdapter;
use crate::adapters::cloudflare::CloudflareAdapter;
use crate::adapters::convex::ConvexAdapter;
use crate::adapters::{SecretValue, SyncAdapter};
use crate::config::{Config, ResolvedTarget};
use crate::store::SecretStore;
use crate::tracker::SyncIndex;

pub fn run(
    config: &Config,
    env: Option<&str>,
    force: bool,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let index_path = config.root.join(".sync-index.json");
    let mut index = SyncIndex::load(&index_path)?;

    let resolved = config.resolve_secrets()?;

    // Collect all (secret, target) pairs that need syncing
    // For env adapter: if ANY secret changed for an (app, env) pair, resync ALL secrets for that pair
    let mut env_dirty_pairs: BTreeSet<(String, String)> = BTreeSet::new(); // (app, env)
    let mut non_env_work: Vec<(String, String, ResolvedTarget)> = Vec::new(); // (key, value, target)

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

            if target.adapter == "env" {
                // For env adapter, mark the (app, env) pair as dirty if any secret changed
                if index.should_sync(&tracker_key, &value_hash, force) {
                    if let Some(app) = &target.app {
                        env_dirty_pairs.insert((app.clone(), target.environment.clone()));
                    }
                }
            } else if target.adapter != "onepassword" {
                // 1Password is handled via push/pull, not sync
                if index.should_sync(&tracker_key, &value_hash, force) {
                    non_env_work.push((secret.key.clone(), value.clone(), target.clone()));
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

    // Handle env adapter: for each dirty (app, env), gather ALL secrets and regenerate
    if !env_dirty_pairs.is_empty() {
        let env_adapter = EnvFileAdapter { config };

        for (app, target_env) in &env_dirty_pairs {
            // Gather all secrets that target this (app, env) via the env adapter
            let mut secrets_for_file: Vec<SecretValue> = Vec::new();

            for secret in &resolved {
                for target in &secret.targets {
                    if target.adapter == "env"
                        && target.app.as_deref() == Some(app.as_str())
                        && target.environment == *target_env
                    {
                        let composite = format!("{}:{}", secret.key, target_env);
                        if let Some(value) = payload.secrets.get(&composite) {
                            secrets_for_file.push(SecretValue {
                                key: secret.key.clone(),
                                value: value.clone(),
                                vendor: secret.vendor.clone(),
                            });
                        }
                    }
                }
            }

            if secrets_for_file.is_empty() {
                continue;
            }

            let target = ResolvedTarget {
                adapter: "env".to_string(),
                app: Some(app.clone()),
                environment: target_env.clone(),
            };

            if dry_run {
                let path = config.resolve_env_path(app, target_env)?;
                println!(
                    "  {} {} ({} secrets) → {}",
                    style("would sync").cyan(),
                    style("env").bold(),
                    secrets_for_file.len(),
                    path.display()
                );
                sync_count += secrets_for_file.len() as u32;
                continue;
            }

            if verbose {
                let path = config.resolve_env_path(app, target_env)?;
                println!(
                    "  {} {} ({} secrets) → {}",
                    style("syncing").cyan(),
                    style("env").bold(),
                    secrets_for_file.len(),
                    path.display()
                );
            }

            let results = env_adapter.sync_batch(&secrets_for_file, &target);

            // Update tracker for ALL secrets in the regenerated file
            for result in &results {
                let tracker_key = SyncIndex::tracker_key(
                    &result.key,
                    "env",
                    Some(app.as_str()),
                    target_env,
                );
                let composite = format!("{}:{}", result.key, target_env);
                let value = payload.secrets.get(&composite).map(|v| v.as_str()).unwrap_or("");
                let value_hash = SyncIndex::hash_value(value);

                if result.success {
                    index.record_success(tracker_key, target.to_string(), value_hash);
                    sync_count += 1;
                } else {
                    let error = result.error.clone().unwrap_or_default();
                    index.record_failure(tracker_key, target.to_string(), value_hash, error.clone());
                    fail_count += 1;
                    eprintln!("  {} {}:{} → env: {}", style("fail").red(), result.key, target_env, error);
                }
            }
        }
    }

    // Handle non-env adapters (cloudflare, convex)
    for (key, value, target) in &non_env_work {
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

        let result = match target.adapter.as_str() {
            "cloudflare" => {
                let adapter_config = config
                    .adapters
                    .cloudflare
                    .as_ref()
                    .context("cloudflare adapter not configured")?;
                let adapter = CloudflareAdapter {
                    config,
                    adapter_config,
                };
                adapter.sync_secret(key, value, target)
            }
            "convex" => {
                let adapter_config = config
                    .adapters
                    .convex
                    .as_ref()
                    .context("convex adapter not configured")?;
                let adapter = ConvexAdapter {
                    config,
                    adapter_config,
                };
                adapter.sync_secret(key, value, target)
            }
            other => {
                anyhow::bail!("unknown adapter '{other}' during sync");
            }
        };

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

    // Also count skipped env secrets
    for secret in &resolved {
        for target in &secret.targets {
            if target.adapter != "env" {
                continue;
            }
            if let Some(filter_env) = env {
                if target.environment != filter_env {
                    continue;
                }
            }
            if let Some(app) = &target.app {
                if !env_dirty_pairs.contains(&(app.clone(), target.environment.clone())) {
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
