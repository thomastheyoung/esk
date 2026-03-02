use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::IsTerminal;

use crate::config::{Config, ResolvedSecret};
use crate::deploy_tracker::DeployIndex;
use crate::store::StorePayload;
use crate::targets::{DeployMode, DeployTarget, SecretValue};
use crate::validate;

use super::report::DeployEntry;
use super::types::{BatchGroup, EnvWorkPlan, PlanOutput, PRUNE_THRESHOLD};
use super::DeployOptions;

pub(crate) fn plan_deploy<'a>(
    config: &Config,
    payload: &StorePayload,
    index: &DeployIndex,
    resolved: &[ResolvedSecret],
    deploy_targets: &[Box<dyn DeployTarget + 'a>],
    target_map: &HashMap<&str, (usize, DeployMode)>,
    opts: &DeployOptions<'_>,
) -> Result<PlanOutput> {
    let DeployOptions {
        env,
        force,
        dry_run,
        skip_validation,
        strict,
        allow_empty,
        prune,
        ..
    } = *opts;

    // -----------------------------------------------------------------------
    // Validation
    // -----------------------------------------------------------------------

    if !skip_validation {
        let mut validation_errors: Vec<String> = Vec::new();
        for secret in resolved {
            let Some(ref spec) = secret.validate else {
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

                if let Err(e) = validate::validate_value(&secret.key, value, spec) {
                    validation_errors
                        .push(format!("  {}:{} — {}", secret.key, target.environment, e));
                }
            }
        }
        if !validation_errors.is_empty() {
            if dry_run {
                for e in &validation_errors {
                    cliclack::log::warning(e)?;
                }
            } else {
                anyhow::bail!(
                    "Validation failed:\n{}\n  Use --skip-validation to bypass",
                    validation_errors.join("\n")
                );
            }
        }
    }

    // Cross-field validation
    if !skip_validation {
        let mut cross_field_specs: BTreeMap<&str, &validate::Validation> = BTreeMap::new();
        for secret in resolved {
            if let Some(ref spec) = secret.validate {
                if spec.has_cross_field_rules() {
                    cross_field_specs.insert(secret.key.as_str(), spec);
                }
            }
        }

        if !cross_field_specs.is_empty() {
            let envs: Vec<&str> = match env {
                Some(e) => vec![e],
                None => config
                    .environments
                    .iter()
                    .map(std::string::String::as_str)
                    .collect(),
            };
            let mut cross_errors: Vec<String> = Vec::new();
            for &env_name in &envs {
                let violations =
                    validate::validate_cross_field(&cross_field_specs, &payload.secrets, env_name);
                for v in violations {
                    cross_errors.push(format!("  {}:{} — {}", v.key, v.env, v.message));
                }
            }
            if !cross_errors.is_empty() {
                if dry_run {
                    for e in &cross_errors {
                        cliclack::log::warning(e)?;
                    }
                } else {
                    anyhow::bail!(
                        "Cross-field validation failed:\n{}\n  Use --skip-validation to bypass",
                        cross_errors.join("\n")
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Required checks
    // -----------------------------------------------------------------------

    let available_targets: Vec<&str> = deploy_targets.iter().map(|t| t.name()).collect();
    let missing =
        config.check_requirements(resolved, &payload.secrets, env, Some(&available_targets));
    let warned_missing: BTreeSet<(String, String)> = missing
        .iter()
        .map(|m| (m.key.clone(), m.env.clone()))
        .collect();
    if !missing.is_empty() {
        if dry_run || !strict {
            for m in &missing {
                cliclack::log::warning(format!("Missing required: {}:{}", m.key, m.env))?;
            }
        }
        if strict && !dry_run && !force {
            let lines: Vec<String> = missing
                .iter()
                .map(|m| format!("  {}:{}", m.key, m.env))
                .collect();
            anyhow::bail!(
                "Required secrets missing:\n{}\n\n  \
                 Set them with:\n{}\n\n  \
                 Use --force to deploy anyway",
                lines.join("\n"),
                missing
                    .iter()
                    .map(|m| format!("  esk set {} --env {}", m.key, m.env))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }

    // -----------------------------------------------------------------------
    // Empty value checks
    // -----------------------------------------------------------------------

    if !allow_empty && !force {
        let mut empty_warnings: Vec<String> = Vec::new();
        for secret in resolved {
            if secret.allow_empty {
                continue;
            }
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

                // Only check secrets that need deploying (changed or never deployed)
                let value_hash = DeployIndex::hash_value(value);
                let tracker_key = DeployIndex::tracker_key(
                    &secret.key,
                    &target.service,
                    target.app.as_deref(),
                    &target.environment,
                );
                // force=false here is intentional: the outer guard already skips
                // this block when --force is set, so the value doesn't matter.
                if !index.should_deploy(&tracker_key, &value_hash, false) {
                    continue;
                }

                if validate::is_effectively_empty(value) {
                    let kind = if value.is_empty() {
                        "empty"
                    } else {
                        "whitespace-only"
                    };
                    empty_warnings.push(format!(
                        "  {}:{} — {}",
                        secret.key, target.environment, kind
                    ));
                }
            }
        }
        if !empty_warnings.is_empty() {
            let detail = empty_warnings.join("\n");
            if dry_run {
                for w in &empty_warnings {
                    cliclack::log::warning(w)?;
                }
            } else if std::io::stdin().is_terminal() {
                cliclack::log::warning(format!("Empty values detected:\n{detail}"))?;
                let proceed = cliclack::confirm(
                    "Empty values can break defaults and type coercion. Continue?",
                )
                .initial_value(false)
                .interact()?;
                if !proceed {
                    anyhow::bail!("Aborted. Use --allow-empty to proceed.");
                }
            } else {
                anyhow::bail!(
                    "Empty values would be deployed:\n{detail}\n  Use --allow-empty to proceed"
                );
            }
        }
    }

    // -----------------------------------------------------------------------
    // Work classification
    // -----------------------------------------------------------------------

    // Track batch-mode dirty target groups: (target_name, app, env)
    let mut batch_dirty: BTreeSet<(String, Option<String>, String)> = BTreeSet::new();
    // Individual-mode work items: (key, value, target)
    let mut individual_work: Vec<(String, String, crate::config::ResolvedTarget)> = Vec::new();

    let mut skipped: Vec<DeployEntry> = Vec::new();
    let mut unset: Vec<DeployEntry> = Vec::new();

    // Orphan detection and prune work collection
    let mut prune_individual: Vec<crate::orphan::TargetOrphan> = Vec::new();
    let mut batch_prune_keys: BTreeMap<
        (String, Option<String>, String),
        Vec<crate::orphan::TargetOrphan>,
    > = BTreeMap::new();
    let mut unavailable_orphans: Vec<crate::orphan::TargetOrphan> = Vec::new();

    if prune {
        let orphans = crate::orphan::detect(index, resolved, env);
        if !orphans.is_empty() {
            if orphans.len() > PRUNE_THRESHOLD && !force {
                anyhow::bail!(
                    "{} orphaned secrets detected (threshold: {PRUNE_THRESHOLD}). \
                     Use --force to override.",
                    orphans.len()
                );
            }

            if !dry_run && std::io::stdin().is_terminal() {
                let lines: Vec<String> = orphans
                    .iter()
                    .map(|o| format!("  {} → {} ({})", o.key, o.target_display(), o.env))
                    .collect();
                cliclack::log::warning(format!(
                    "Orphaned secrets to prune:\n{}",
                    lines.join("\n")
                ))?;
                let proceed = cliclack::confirm("Remove these orphaned secrets from targets?")
                    .initial_value(true)
                    .interact()?;
                if !proceed {
                    anyhow::bail!("Prune aborted.");
                }
            }

            for orphan in orphans {
                if let Some((_, deploy_mode)) = target_map.get(orphan.service.as_str()) {
                    match deploy_mode {
                        DeployMode::Batch => {
                            batch_dirty.insert((
                                orphan.service.clone(),
                                orphan.app.clone(),
                                orphan.env.clone(),
                            ));
                            batch_prune_keys
                                .entry((
                                    orphan.service.clone(),
                                    orphan.app.clone(),
                                    orphan.env.clone(),
                                ))
                                .or_default()
                                .push(orphan);
                        }
                        DeployMode::Individual => {
                            prune_individual.push(orphan);
                        }
                    }
                } else {
                    unavailable_orphans.push(orphan);
                }
            }
        }
    }

    for secret in resolved {
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
            let Some(value) = payload.secrets.get(&composite) else {
                // Skip unset entries already warned as missing required
                if !warned_missing.contains(&(secret.key.clone(), target.environment.clone())) {
                    unset.push(DeployEntry {
                        key: secret.key.clone(),
                        env: target.environment.clone(),
                        target: target.target_display(),
                        error: None,
                    });
                }
                continue;
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
                            target: target.target_display(),
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
        for secret in resolved {
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

    // -----------------------------------------------------------------------
    // Build per-environment work plans
    // -----------------------------------------------------------------------

    // Collect tombstone work for individual targets
    let mut tombstone_work: Vec<(String, crate::config::ResolvedTarget)> = Vec::new();
    for composite_key in payload.tombstones.keys() {
        let Some((bare_key, tomb_env)) = composite_key.rsplit_once(':') else {
            continue;
        };
        if let Some(filter_env) = env {
            if tomb_env != filter_env {
                continue;
            }
        }
        for secret in resolved {
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
                if !force && !index.should_deploy(&tracker_key, DeployIndex::TOMBSTONE_HASH, false)
                {
                    continue;
                }
                tombstone_work.push((bare_key.to_string(), target.clone()));
            }
        }
    }

    let mut env_plans: BTreeMap<String, EnvWorkPlan> = BTreeMap::new();

    // Insert batch groups
    for (target_name, app, target_env) in &batch_dirty {
        let (target_idx, _) = target_map[target_name.as_str()];
        let mut secrets_for_batch: Vec<SecretValue> = Vec::new();
        let mut tombstoned_keys: BTreeSet<String> = BTreeSet::new();
        for secret in resolved {
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
                            group: secret.group.clone(),
                        });
                    }
                }
            }
        }
        let plan = env_plans.entry(target_env.clone()).or_default();
        plan.batch_groups.push(BatchGroup {
            target_name: target_name.clone(),
            app: app.clone(),
            secrets: secrets_for_batch,
            tombstoned_keys,
            target_idx,
        });
    }

    // Insert individual work
    for (key, value, target) in &individual_work {
        let plan = env_plans.entry(target.environment.clone()).or_default();
        plan.individual
            .push((key.clone(), value.clone(), target.clone()));
    }

    // Insert tombstone work
    for (key, target) in &tombstone_work {
        let plan = env_plans.entry(target.environment.clone()).or_default();
        plan.tombstones.push((key.clone(), target.clone()));
    }

    // Insert prune work
    for orphan in &prune_individual {
        let plan = env_plans.entry(orphan.env.clone()).or_default();
        plan.prune_individual.push(orphan.clone());
    }

    // Insert batch prune work
    for ((target_name, app, target_env), orphan_list) in &batch_prune_keys {
        let plan = env_plans.entry(target_env.clone()).or_default();
        plan.batch_prune
            .entry((target_name.clone(), app.clone()))
            .or_default()
            .extend(orphan_list.iter().cloned());
    }

    // Also collect environments that only have unset or skipped secrets
    for entry in &unset {
        env_plans.entry(entry.env.clone()).or_default();
    }

    // Count skipped batch secrets (those in non-dirty batch target groups)
    for secret in resolved {
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
                            target: target.target_display(),
                            error: None,
                        });
                    }
                }
            }
        }
    }

    Ok(PlanOutput {
        env_plans,
        unset,
        skipped,
        unavailable_orphans,
    })
}
