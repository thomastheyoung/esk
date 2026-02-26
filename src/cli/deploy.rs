use anyhow::Result;
use console::style;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::config::Config;
use crate::deploy_tracker::DeployIndex;
use crate::store::SecretStore;
use crate::targets::{build_targets, CommandRunner, DeployMode, RealCommandRunner, SecretValue};
use crate::ui;

/// A single deploy result entry for display.
struct DeployEntry {
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
    skip_validation: bool,
) -> Result<()> {
    run_with_runner(
        config,
        env,
        force,
        dry_run,
        verbose,
        skip_validation,
        &RealCommandRunner,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn run_with_runner(
    config: &Config,
    env: Option<&str>,
    force: bool,
    dry_run: bool,
    verbose: bool,
    skip_validation: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let index_path = config.root.join(".esk/deploy-index.json");
    let mut index = DeployIndex::load(&index_path);

    let resolved = config.resolve_secrets()?;

    let has_configured_targets = !config.target_names().is_empty();

    let deploy_targets = build_targets(config, runner);

    if deploy_targets.is_empty() && has_configured_targets {
        cliclack::log::warning(
            "No targets available after preflight checks. Fix the issues above and try again.",
        )?;
        return Ok(());
    }

    // Validate changed secrets before deploying
    if !skip_validation {
        let mut validation_errors: Vec<String> = Vec::new();
        for secret in &resolved {
            let Some(ref spec) = secret.validation else {
                continue;
            };
            for target in &secret.targets {
                if let Some(filter_env) = env {
                    if target.environment != filter_env {
                        continue;
                    }
                }
                let composite = format!("{}:{}", secret.key, target.environment);
                let Some(value) = payload.secrets.get(&composite) else {
                    continue;
                };

                // Only validate if this secret needs deploying (changed or never deployed)
                let value_hash = DeployIndex::hash_value(value);
                let tracker_key = DeployIndex::tracker_key(
                    &secret.key,
                    &target.service,
                    target.app.as_deref(),
                    &target.environment,
                );
                if !index.should_deploy(&tracker_key, &value_hash, force) {
                    continue;
                }

                if let Err(e) = crate::validate::validate_value(&secret.key, value, spec) {
                    validation_errors
                        .push(format!("  {}:{} — {}", secret.key, target.environment, e));
                }
            }
        }
        if !validation_errors.is_empty() {
            anyhow::bail!(
                "Validation failed:\n{}\n  Use --skip-validation to bypass",
                validation_errors.join("\n")
            );
        }
    }

    // Build a lookup map: target_name -> (index, deploy_mode)
    let target_map: HashMap<&str, (usize, DeployMode)> = deploy_targets
        .iter()
        .enumerate()
        .map(|(i, a)| (a.name(), (i, a.deploy_mode())))
        .collect();

    // Track batch-mode dirty target groups: (target_name, app, env)
    let mut batch_dirty: BTreeSet<(String, Option<String>, String)> = BTreeSet::new();
    // Individual-mode work items: (key, value, target)
    let mut individual_work: Vec<(String, String, crate::config::ResolvedTarget)> = Vec::new();

    // Collect structured results
    let mut deployed: Vec<DeployEntry> = Vec::new();
    let mut skipped: Vec<DeployEntry> = Vec::new();
    let mut failed: Vec<DeployEntry> = Vec::new();
    let mut unset: Vec<DeployEntry> = Vec::new();

    for secret in &resolved {
        for target in &secret.targets {
            // Filter by environment if specified
            if let Some(filter_env) = env {
                if target.environment != filter_env {
                    continue;
                }
            }

            // Skip targets whose target isn't in the deploy target map (e.g. remotes)
            let (_, deploy_mode) = match target_map.get(target.service.as_str()) {
                Some(entry) => *entry,
                None => continue,
            };

            let composite = format!("{}:{}", secret.key, target.environment);
            let value = match payload.secrets.get(&composite) {
                Some(v) => v,
                None => {
                    unset.push(DeployEntry {
                        key: secret.key.clone(),
                        env: target.environment.clone(),
                        target: format_target(target),
                        error: None,
                    });
                    if verbose {
                        cliclack::log::remark(format!(
                            "{}:{} — no value set",
                            secret.key, target.environment
                        ))?;
                    }
                    continue;
                }
            };

            let value_hash = DeployIndex::hash_value(value);
            let tracker_key = DeployIndex::tracker_key(
                &secret.key,
                &target.service,
                target.app.as_deref(),
                &target.environment,
            );

            match deploy_mode {
                DeployMode::Batch => {
                    if index.should_deploy(&tracker_key, &value_hash, force) {
                        batch_dirty.insert((
                            target.service.clone(),
                            target.app.clone(),
                            target.environment.clone(),
                        ));
                    }
                }
                DeployMode::Individual => {
                    if index.should_deploy(&tracker_key, &value_hash, force) {
                        individual_work.push((secret.key.clone(), value.clone(), target.clone()));
                    } else {
                        skipped.push(DeployEntry {
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
                if let Some((_, DeployMode::Batch)) = target_map.get(target.service.as_str()) {
                    batch_dirty.insert((
                        target.service.clone(),
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
        s.start("Deploying secrets...");
        Some(s)
    } else {
        None
    };

    // Handle batch targets: for each dirty target group, gather ALL secrets and deploy
    for (target_name, app, target_env) in &batch_dirty {
        let (target_idx, _) = target_map[target_name.as_str()];
        let deploy_target = &deploy_targets[target_idx];
        let target = crate::config::ResolvedTarget {
            service: target_name.clone(),
            app: app.clone(),
            environment: target_env.clone(),
        };
        let target_display = format_target(&target);

        // Gather all secrets that target this (target, app, env)
        let mut secrets_for_batch: Vec<SecretValue> = Vec::new();
        let mut tombstoned_keys: BTreeSet<String> = BTreeSet::new();
        for secret in &resolved {
            for target in &secret.targets {
                if target.service == *target_name
                    && target.app.as_ref() == app.as_ref()
                    && target.environment == *target_env
                {
                    let composite = format!("{}:{}", secret.key, target_env);
                    if payload.tombstones.contains_key(&composite) {
                        tombstoned_keys.insert(secret.key.clone());
                    }
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

        if dry_run {
            if secrets_for_batch.is_empty() {
                for key in &tombstoned_keys {
                    deployed.push(DeployEntry {
                        key: key.clone(),
                        env: target_env.clone(),
                        target: target_display.clone(),
                        error: None,
                    });
                }
                continue;
            }
            for s in &secrets_for_batch {
                deployed.push(DeployEntry {
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
                "Deploying {} ({} secrets) → {}",
                style(target_name).bold(),
                secrets_for_batch.len(),
                target
            ))?;
        }

        let results = deploy_target.deploy_batch(&secrets_for_batch, &target);
        if results.is_empty() {
            for key in &tombstoned_keys {
                let tracker_key =
                    DeployIndex::tracker_key(key, target_name, app.as_deref(), target_env);
                index.record_success(
                    tracker_key,
                    target.to_string(),
                    DeployIndex::TOMBSTONE_HASH.to_string(),
                );
                deployed.push(DeployEntry {
                    key: key.clone(),
                    env: target_env.clone(),
                    target: target_display.clone(),
                    error: None,
                });
            }
            // Save index after each batch group
            index.save()?;
            continue;
        }

        for result in &results {
            let tracker_key =
                DeployIndex::tracker_key(&result.key, target_name, app.as_deref(), target_env);
            let composite = format!("{}:{}", result.key, target_env);
            let value = payload
                .secrets
                .get(&composite)
                .map(|v| v.as_str())
                .unwrap_or("");
            let value_hash = DeployIndex::hash_value(value);

            if result.success {
                index.record_success(tracker_key, target.to_string(), value_hash);
                deployed.push(DeployEntry {
                    key: result.key.clone(),
                    env: target_env.clone(),
                    target: target_display.clone(),
                    error: None,
                });
            } else {
                let error = result.error.clone().unwrap_or_default();
                index.record_failure(tracker_key, target.to_string(), value_hash, error.clone());
                failed.push(DeployEntry {
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

    // Handle individual targets
    for (key, value, target) in &individual_work {
        let target_display = format_target(target);

        if dry_run {
            deployed.push(DeployEntry {
                key: key.clone(),
                env: target.environment.clone(),
                target: target_display,
                error: None,
            });
            continue;
        }

        if verbose {
            cliclack::log::step(format!(
                "Deploying {}:{} → {}",
                key, target.environment, target
            ))?;
        }

        let (target_idx, _) = target_map[target.service.as_str()];
        let deploy_target = &deploy_targets[target_idx];
        let result = deploy_target.deploy_secret(key, value, target);

        let tracker_key = DeployIndex::tracker_key(
            key,
            &target.service,
            target.app.as_deref(),
            &target.environment,
        );
        let value_hash = DeployIndex::hash_value(value);

        match result {
            Ok(()) => {
                index.record_success(tracker_key, target.to_string(), value_hash);
                deployed.push(DeployEntry {
                    key: key.clone(),
                    env: target.environment.clone(),
                    target: target_display,
                    error: None,
                });
                if verbose {
                    cliclack::log::success(format!(
                        "Deployed {}:{} → {}",
                        key, target.environment, target
                    ))?;
                }
            }
            Err(e) => {
                index.record_failure(tracker_key, target.to_string(), value_hash, e.to_string());
                failed.push(DeployEntry {
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

    // Process tombstones: delete secrets from individual targets
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
                let Some((_, DeployMode::Individual)) = target_map.get(target.service.as_str())
                else {
                    continue;
                };

                let tracker_key = DeployIndex::tracker_key(
                    bare_key,
                    &target.service,
                    target.app.as_deref(),
                    tomb_env,
                );

                // Skip if already successfully deleted (unless forced)
                if !force && !index.should_deploy(&tracker_key, DeployIndex::TOMBSTONE_HASH, false)
                {
                    continue;
                }

                if dry_run {
                    deployed.push(DeployEntry {
                        key: bare_key.to_string(),
                        env: tomb_env.to_string(),
                        target: format_target(target),
                        error: None,
                    });
                    continue;
                }

                let (target_idx, _) = target_map[target.service.as_str()];
                let deploy_target = &deploy_targets[target_idx];

                match deploy_target.delete_secret(bare_key, target) {
                    Ok(()) => {
                        index.record_success(
                            tracker_key,
                            format_target(target),
                            DeployIndex::TOMBSTONE_HASH.to_string(),
                        );
                        deployed.push(DeployEntry {
                            key: bare_key.to_string(),
                            env: tomb_env.to_string(),
                            target: format_target(target),
                            error: None,
                        });
                    }
                    Err(e) => {
                        index.record_failure(
                            tracker_key,
                            format_target(target),
                            DeployIndex::TOMBSTONE_HASH.to_string(),
                            e.to_string(),
                        );
                        failed.push(DeployEntry {
                            key: bare_key.to_string(),
                            env: tomb_env.to_string(),
                            target: format_target(target),
                            error: Some(e.to_string()),
                        });
                    }
                }
                index.save()?;
            }
        }
    }

    // Count skipped batch secrets (those in non-dirty batch target groups)
    for secret in &resolved {
        for target in &secret.targets {
            if let Some((_, DeployMode::Batch)) = target_map.get(target.service.as_str()) {
                if let Some(filter_env) = env {
                    if target.environment != filter_env {
                        continue;
                    }
                }
                let group = (
                    target.service.clone(),
                    target.app.clone(),
                    target.environment.clone(),
                );
                if !batch_dirty.contains(&group) {
                    let composite = format!("{}:{}", secret.key, target.environment);
                    if payload.secrets.contains_key(&composite) {
                        skipped.push(DeployEntry {
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

    let deploy_count = deployed.len();
    let skip_count = skipped.len();
    let fail_count = failed.len();
    let unset_count = unset.len();

    // Stop spinner before printing results
    if let Some(s) = spinner {
        s.stop("Deploying secrets...");
    }

    // Output
    let width = 44;

    if deploy_count == 0 && skip_count == 0 && fail_count == 0 && unset_count == 0 {
        cliclack::log::info("Nothing to deploy.")?;
    } else {
        // Group everything by environment for framed output
        let mut env_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut env_status: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new(); // (deployed, failed, unset)

        for entry in &deployed {
            let label = format!("{} {}", style("✔").green(), style(&entry.key).dim());
            env_map
                .entry(entry.env.clone())
                .or_default()
                .push(ui::format_dashboard_line(&label, &entry.target, width));
            env_status.entry(entry.env.clone()).or_insert((0, 0, 0)).0 += 1;
        }

        for entry in &failed {
            let label = format!("{} {}", style("✗").red(), style(&entry.key).dim());
            let lines = env_map.entry(entry.env.clone()).or_default();
            lines.push(ui::format_dashboard_line(&label, &entry.target, width));
            if let Some(err) = &entry.error {
                lines.push(format!("      {}", style(format!("({err})")).red().dim()));
            }
            env_status.entry(entry.env.clone()).or_insert((0, 0, 0)).1 += 1;
        }

        for entry in &unset {
            let label = format!("{} {}", style("○").dim(), style(&entry.key).dim());
            env_map
                .entry(entry.env.clone())
                .or_default()
                .push(ui::format_dashboard_line(&label, &entry.target, width));
            env_status.entry(entry.env.clone()).or_insert((0, 0, 0)).2 += 1;
        }

        for (env_name, mut lines) in env_map {
            let (s_cnt, f_cnt, u_cnt) = env_status.get(&env_name).unwrap();
            let mut status_parts = Vec::new();
            if *f_cnt > 0 {
                status_parts.push(format!("{} failed", f_cnt));
            }
            if *s_cnt > 0 {
                status_parts.push(format!("{} deployed", s_cnt));
            }
            if *u_cnt > 0 {
                status_parts.push(format!("{} unset", u_cnt));
            }

            lines.push(String::new());
            let status_icon = if *f_cnt > 0 {
                style("✗").red()
            } else {
                style("✔").green()
            };
            lines.push(format!(
                "{} Deployment complete ({})",
                status_icon,
                status_parts.join(", ")
            ));

            cliclack::note(env_name, lines.join("\n"))?;
        }

        if skip_count > 0 {
            if verbose {
                let mut skip_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for entry in &skipped {
                    let label = format!("{} {}", style("✔").dim(), style(&entry.key).dim());
                    skip_map
                        .entry(entry.env.clone())
                        .or_default()
                        .push(ui::format_dashboard_line(
                            &label,
                            &format!("{} (up to date)", entry.target),
                            width,
                        ));
                }
                for (env_name, lines) in skip_map {
                    cliclack::note(format!("{} (up to date)", env_name), lines.join("\n"))?;
                }
            } else {
                cliclack::log::remark(format!(
                    "{} targets up to date  {}",
                    style(skip_count).bold(),
                    style("(use --verbose to show)").dim()
                ))?;
            }
        }
    }

    if dry_run {
        cliclack::log::warning("Dry run — no changes made".to_string())?;
    }

    if fail_count > 0 {
        anyhow::bail!("{fail_count} deploy(s) failed");
    }

    Ok(())
}

fn format_target(target: &crate::config::ResolvedTarget) -> String {
    let mut s = target.service.clone();
    if let Some(app) = &target.app {
        s.push(':');
        s.push_str(app);
    }
    s
}
