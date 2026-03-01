use anyhow::Result;
use console::style;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::IsTerminal;
use std::sync::Mutex;

use crate::config::Config;
use crate::deploy_tracker::DeployIndex;
use crate::store::SecretStore;
use crate::targets::{build_targets, CommandRunner, DeployMode, RealCommandRunner, SecretValue};
use crate::ui;
use crate::validate;

/// Options for the deploy command.
pub struct DeployOptions<'a> {
    pub env: Option<&'a str>,
    pub force: bool,
    pub dry_run: bool,
    pub verbose: bool,
    pub skip_validation: bool,
    pub strict: bool,
    pub allow_empty: bool,
    pub prune: bool,
}

/// Maximum number of orphans allowed without `--force`.
const PRUNE_THRESHOLD: usize = 10;

const DEPLOY_LINE_WIDTH: usize = 20;

#[derive(Default)]
struct EnvStatus {
    deployed: usize,
    failed: usize,
    unset: usize,
    pruned: usize,
}

/// A single deploy result entry for display.
struct DeployEntry {
    key: String,
    env: String,
    target: String,
    error: Option<String>,
}

struct DeployReport {
    deployed: Vec<DeployEntry>,
    failed: Vec<DeployEntry>,
    skipped: Vec<DeployEntry>,
    unset: Vec<DeployEntry>,
    pruned: Vec<DeployEntry>,
    dry_run: bool,
    verbose: bool,
}

impl DeployReport {
    fn is_empty(&self) -> bool {
        self.deployed.is_empty()
            && self.failed.is_empty()
            && self.skipped.is_empty()
            && self.unset.is_empty()
            && self.pruned.is_empty()
    }

    fn has_failures(&self) -> bool {
        !self.failed.is_empty()
    }

    fn all_entries(&self) -> impl Iterator<Item = &DeployEntry> {
        self.deployed
            .iter()
            .chain(&self.failed)
            .chain(&self.skipped)
            .chain(&self.unset)
            .chain(&self.pruned)
    }

    fn render(&self) -> Result<()> {
        if self.is_empty() {
            cliclack::log::info("Nothing to deploy.")?;
        } else {
            // Compute label column: max(MIN_WIDTH, longest_key + icon_prefix + 3 dots)
            let max_key_len = self.all_entries().map(|e| e.key.len()).max().unwrap_or(0);
            let label_col = DEPLOY_LINE_WIDTH.max(max_key_len + 7); // +2 icon, +2 spaces, +3 min dots

            let mut env_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let mut env_status: BTreeMap<String, EnvStatus> = BTreeMap::new();

            // Count statuses from original entries (one per target)
            for entry in &self.deployed {
                env_status.entry(entry.env.clone()).or_default().deployed += 1;
            }
            for entry in &self.failed {
                env_status.entry(entry.env.clone()).or_default().failed += 1;
            }
            for entry in &self.unset {
                env_status.entry(entry.env.clone()).or_default().unset += 1;
            }
            for entry in &self.pruned {
                env_status.entry(entry.env.clone()).or_default().pruned += 1;
            }

            // Render deployed entries (grouped by key, targets on one line)
            for ((env, key), (targets, _)) in group_entries(&self.deployed) {
                let label = format!("{} {}", ui::icon_success(), style(&key).dim());
                env_map
                    .entry(env)
                    .or_default()
                    .push(ui::format_aligned_line(
                        &label,
                        &targets.join(", "),
                        label_col,
                    ));
            }

            // Render failed entries (grouped by key, with errors)
            for ((env, key), (targets, errors)) in group_entries(&self.failed) {
                let label = format!("{} {}", ui::icon_failure(), style(&key).dim());
                let lines = env_map.entry(env).or_default();
                lines.push(ui::format_aligned_line(
                    &label,
                    &targets.join(", "),
                    label_col,
                ));
                if !errors.is_empty() {
                    let unique_errors: BTreeSet<&str> =
                        errors.iter().map(|(_, e)| e.as_str()).collect();
                    if unique_errors.len() == 1 {
                        let err = unique_errors.into_iter().next().unwrap();
                        lines.push(format!("      {}", style(format!("({err})")).red().dim()));
                    } else {
                        for (target, err) in &errors {
                            lines.push(format!(
                                "      {}",
                                style(format!("{target}: ({err})")).red().dim()
                            ));
                        }
                    }
                }
            }

            // Render unset entries (grouped by key)
            for ((env, key), (targets, _)) in group_entries(&self.unset) {
                let label = format!("{} {}", ui::icon_unset(), style(&key).dim());
                env_map
                    .entry(env)
                    .or_default()
                    .push(ui::format_aligned_line(
                        &label,
                        &targets.join(", "),
                        label_col,
                    ));
            }

            // Render pruned entries (grouped by key)
            for ((env, key), (targets, _)) in group_entries(&self.pruned) {
                let label = format!("{} {}", ui::icon_pruned(), style(&key).dim());
                env_map
                    .entry(env)
                    .or_default()
                    .push(ui::format_aligned_line(
                        &label,
                        &format!("{} (pruned)", targets.join(", ")),
                        label_col,
                    ));
            }

            for (env_name, mut lines) in env_map {
                let es = env_status.get(&env_name).unwrap();
                let status_summary = ui::format_count_summary(&[
                    ("failed", es.failed),
                    ("deployed", es.deployed),
                    ("unset", es.unset),
                    ("pruned", es.pruned),
                ]);

                lines.push(String::new());
                let status_icon = if es.failed > 0 {
                    ui::icon_failure()
                } else {
                    ui::icon_success()
                };
                lines.push(format!(
                    "{status_icon} Deployment complete ({status_summary})"
                ));

                cliclack::note(env_name, lines.join("\n"))?;
            }

            if !self.skipped.is_empty() {
                if self.verbose {
                    let mut skip_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
                    for ((env, key), (targets, _)) in group_entries(&self.skipped) {
                        let label = format!("{} {}", style("✔").dim(), style(&key).dim());
                        skip_map
                            .entry(env)
                            .or_default()
                            .push(ui::format_aligned_line(
                                &label,
                                &targets.join(", "),
                                label_col,
                            ));
                    }
                    for (env_name, lines) in skip_map {
                        cliclack::note(format!("{env_name} (up to date)"), lines.join("\n"))?;
                    }
                } else {
                    let skip_count = self.skipped.len();
                    cliclack::log::remark(format!(
                        "{} targets up to date  {}",
                        style(skip_count).bold(),
                        style("(use --verbose to show)").dim()
                    ))?;
                }
            }
        }

        if self.dry_run {
            cliclack::log::warning("Dry run — no changes made".to_string())?;
        }

        Ok(())
    }

    fn render_skipped(&self) -> Result<()> {
        if self.skipped.is_empty() {
            return Ok(());
        }
        let max_key_len = self.all_entries().map(|e| e.key.len()).max().unwrap_or(0);
        let label_col = DEPLOY_LINE_WIDTH.max(max_key_len + 7);

        let mut skip_map: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for ((env, key), (targets, _)) in group_entries(&self.skipped) {
            let label = format!("{} {}", style("✔").dim(), style(&key).dim());
            skip_map
                .entry(env)
                .or_default()
                .push(ui::format_aligned_line(
                    &label,
                    &targets.join(", "),
                    label_col,
                ));
        }
        for (env_name, lines) in skip_map {
            cliclack::note(format!("{env_name} (up to date)"), lines.join("\n"))?;
        }
        Ok(())
    }
}

/// Targets list and per-target errors for a grouped deploy key.
type GroupedTargets = (Vec<String>, Vec<(String, String)>);

/// Group deploy entries by (env, key), combining targets into lists.
fn group_entries(entries: &[DeployEntry]) -> BTreeMap<(String, String), GroupedTargets> {
    let mut map: BTreeMap<(String, String), GroupedTargets> = BTreeMap::new();
    for entry in entries {
        let group = map
            .entry((entry.env.clone(), entry.key.clone()))
            .or_default();
        group.0.push(entry.target.clone());
        if let Some(ref err) = entry.error {
            group.1.push((entry.target.clone(), err.clone()));
        }
    }
    map
}

// -----------------------------------------------------------------------
// Per-environment deploy plan types
// -----------------------------------------------------------------------

struct BatchGroup {
    target_name: String,
    app: Option<String>,
    secrets: Vec<SecretValue>,
    tombstoned_keys: BTreeSet<String>,
    target_idx: usize,
}

struct EnvWorkPlan {
    batch_groups: Vec<BatchGroup>,
    individual: Vec<(String, String, crate::config::ResolvedTarget)>,
    tombstones: Vec<(String, crate::config::ResolvedTarget)>,
    prune_individual: Vec<crate::orphan::TargetOrphan>,
    batch_prune: BTreeMap<(String, Option<String>), Vec<crate::orphan::TargetOrphan>>,
}

/// A single display line: one key with all its target names.
struct KeyLine {
    key: String,
    targets: Vec<String>,
    total_ops: usize,
}

#[derive(Default)]
struct KeyResult {
    completed_ops: usize,
    total_ops: usize,
    failed: Vec<(String, String)>, // (target_display, error)
}

impl KeyResult {
    fn is_done(&self) -> bool {
        self.completed_ops >= self.total_ops
    }
    fn has_failure(&self) -> bool {
        !self.failed.is_empty()
    }
}

fn build_key_lines(plan: &EnvWorkPlan, unset_entries: &[&DeployEntry]) -> Vec<KeyLine> {
    // Map key -> (set of target display names, op count)
    let mut map: BTreeMap<String, (Vec<String>, usize)> = BTreeMap::new();

    for bg in &plan.batch_groups {
        let display = crate::config::format_target_label(&bg.target_name, bg.app.as_deref());
        for sv in &bg.secrets {
            let entry = map.entry(sv.key.clone()).or_default();
            if !entry.0.contains(&display) {
                entry.0.push(display.clone());
            }
            entry.1 += 1;
        }
        for key in &bg.tombstoned_keys {
            let entry = map.entry(key.clone()).or_default();
            if !entry.0.contains(&display) {
                entry.0.push(display.clone());
            }
            entry.1 += 1;
        }
    }

    for (key, _, target) in &plan.individual {
        let display = target.target_display();
        let entry = map.entry(key.clone()).or_default();
        if !entry.0.contains(&display) {
            entry.0.push(display.clone());
        }
        entry.1 += 1;
    }

    for (key, target) in &plan.tombstones {
        let display = target.target_display();
        let entry = map.entry(key.clone()).or_default();
        if !entry.0.contains(&display) {
            entry.0.push(display.clone());
        }
        entry.1 += 1;
    }

    for orphan in &plan.prune_individual {
        let display = orphan.target_display();
        let entry = map.entry(orphan.key.clone()).or_default();
        if !entry.0.contains(&display) {
            entry.0.push(display.clone());
        }
        entry.1 += 1;
    }

    for orphan_list in plan.batch_prune.values() {
        for orphan in orphan_list {
            let display = orphan.target_display();
            let entry = map.entry(orphan.key.clone()).or_default();
            if !entry.0.contains(&display) {
                entry.0.push(display.clone());
            }
            entry.1 += 1;
        }
    }

    // Add unset keys (0 ops — shown with ○)
    for entry in unset_entries {
        map.entry(entry.key.clone()).or_default();
    }

    map.into_iter()
        .map(|(key, (targets, total_ops))| KeyLine {
            key,
            targets,
            total_ops,
        })
        .collect()
}

fn new_env_work_plan() -> EnvWorkPlan {
    EnvWorkPlan {
        batch_groups: Vec::new(),
        individual: Vec::new(),
        tombstones: Vec::new(),
        prune_individual: Vec::new(),
        batch_prune: BTreeMap::new(),
    }
}

pub fn run(config: &Config, opts: &DeployOptions<'_>) -> Result<()> {
    run_with_runner(config, opts, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    opts: &DeployOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let DeployOptions {
        env,
        force,
        dry_run,
        verbose,
        skip_validation,
        strict,
        allow_empty,
        prune,
    } = *opts;

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let index_path = config.root.join(".esk/deploy-index.json");
    let index = DeployIndex::load(&index_path);

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

                if let Err(e) = crate::validate::validate_value(&secret.key, value, spec) {
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
        for secret in &resolved {
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

    // Check required secrets have values (only for available targets)
    let available_targets: Vec<&str> = deploy_targets.iter().map(|t| t.name()).collect();
    let missing =
        config.check_requirements(&resolved, &payload.secrets, env, Some(&available_targets));
    let warned_missing: BTreeSet<(String, String)> = missing
        .iter()
        .map(|m| (m.key.clone(), m.env.clone()))
        .collect();
    if !missing.is_empty() {
        if dry_run || !strict {
            for m in &missing {
                cliclack::log::warning(format!("Missing required: {}:{}", m.key, m.env,))?;
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

    // Check for empty/whitespace-only values that would be deployed
    if !allow_empty && !force {
        let mut empty_warnings: Vec<String> = Vec::new();
        for secret in &resolved {
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

                if crate::validate::is_effectively_empty(value) {
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
    let mut pruned: Vec<DeployEntry> = Vec::new();

    // Batch groups that had at least one failure (skip pruning for these)
    let failed_batch_groups: BTreeSet<(String, Option<String>, String)> = BTreeSet::new();

    // Orphan detection and prune work collection
    let mut prune_individual: Vec<crate::orphan::TargetOrphan> = Vec::new();
    let mut batch_prune_keys: BTreeMap<
        (String, Option<String>, String),
        Vec<crate::orphan::TargetOrphan>,
    > = BTreeMap::new();
    let mut unavailable_orphans: Vec<crate::orphan::TargetOrphan> = Vec::new();

    if prune {
        let orphans = crate::orphan::detect(&index, &resolved, env);
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

    // -----------------------------------------------------------------------
    // Group work items by environment
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
                if !force && !index.should_deploy(&tracker_key, DeployIndex::TOMBSTONE_HASH, false)
                {
                    continue;
                }
                tombstone_work.push((bare_key.to_string(), target.clone()));
            }
        }
    }

    // Build per-environment work plans
    let mut env_plans: BTreeMap<String, EnvWorkPlan> = BTreeMap::new();

    // Insert batch groups
    for (target_name, app, target_env) in &batch_dirty {
        let (target_idx, _) = target_map[target_name.as_str()];
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
                            group: secret.group.clone(),
                        });
                    }
                }
            }
        }
        let plan = env_plans.entry(target_env.clone()).or_insert_with(new_env_work_plan);
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
        let plan = env_plans.entry(target.environment.clone()).or_insert_with(new_env_work_plan);
        plan.individual.push((key.clone(), value.clone(), target.clone()));
    }

    // Insert tombstone work
    for (key, target) in &tombstone_work {
        let plan = env_plans.entry(target.environment.clone()).or_insert_with(new_env_work_plan);
        plan.tombstones.push((key.clone(), target.clone()));
    }

    // Insert prune work
    for orphan in &prune_individual {
        let plan = env_plans.entry(orphan.env.clone()).or_insert_with(new_env_work_plan);
        plan.prune_individual.push(orphan.clone());
    }

    // Insert batch prune work
    for ((target_name, app, target_env), orphan_list) in &batch_prune_keys {
        let plan = env_plans.entry(target_env.clone()).or_insert_with(new_env_work_plan);
        plan.batch_prune
            .entry((target_name.clone(), app.clone()))
            .or_default()
            .extend(orphan_list.iter().cloned());
    }

    // Also collect environments that only have unset or skipped secrets
    for entry in &unset {
        env_plans.entry(entry.env.clone()).or_insert_with(new_env_work_plan);
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
                            target: target.target_display(),
                            error: None,
                        });
                    }
                }
            }
        }
    }

    // Determine rendering mode
    let is_tty = std::io::stderr().is_terminal();
    let animated = !verbose && !dry_run && is_tty;

    // -----------------------------------------------------------------------
    // Deploy per environment
    // -----------------------------------------------------------------------

    let index = Mutex::new(index);
    let deploy_targets = &deploy_targets;
    let target_map = &target_map;
    let payload_secrets = &payload.secrets;
    let failed_batch_groups = Mutex::new(failed_batch_groups);

    for (env_name, plan) in &env_plans {
        // Group unset entries for this env
        let env_unset: Vec<&DeployEntry> = unset.iter().filter(|e| e.env == *env_name).collect();

        let key_lines = build_key_lines(plan, &env_unset);
        let has_work = plan.batch_groups.iter().any(|bg| !bg.secrets.is_empty() || !bg.tombstoned_keys.is_empty())
            || !plan.individual.is_empty()
            || !plan.tombstones.is_empty()
            || !plan.prune_individual.is_empty()
            || !plan.batch_prune.is_empty();

        if !has_work && env_unset.is_empty() {
            continue;
        }

        // Compute label column for dot-alignment
        let max_key_len = key_lines.iter().map(|kl| kl.key.len()).max().unwrap_or(0);
        let label_col = DEPLOY_LINE_WIDTH.max(max_key_len + 7);

        if animated && has_work {
            // ---------------------------------------------------------------
            // Animated per-secret spinner display
            // ---------------------------------------------------------------

            let n = key_lines.len();
            let results: Mutex<BTreeMap<String, KeyResult>> = Mutex::new(BTreeMap::new());

            // Initialize results
            {
                let mut r = results.lock().unwrap();
                for kl in &key_lines {
                    r.insert(kl.key.clone(), KeyResult {
                        completed_ops: 0,
                        total_ops: kl.total_ops,
                        failed: Vec::new(),
                    });
                }
            }

            let term = console::Term::stderr();
            let frames = ui::SPINNER_FRAMES;
            let bar = style("\u{2502}").dim();

            // Print header + initial spinner lines
            let _ = term.write_line(&format!("{}  {}", style("\u{25C7}").dim(), env_name));
            for kl in &key_lines {
                if kl.total_ops == 0 {
                    // Unset key — show immediately
                    let label = format!("{} {}", ui::icon_unset(), style(&kl.key).dim());
                    let _ = term.write_line(&format!(
                        "{bar}    {}",
                        ui::format_aligned_line(&label, "", label_col)
                    ));
                } else {
                    let label = format!("{} {}", style(frames[0]).magenta(), style(&kl.key).dim());
                    let targets_str = kl.targets.join(", ");
                    let _ = term.write_line(&format!(
                        "{bar}    {}",
                        ui::format_aligned_line(&label, &targets_str, label_col)
                    ));
                }
            }

            // Spawn workers and run animated render loop
            std::thread::scope(|s| {
                // Batch group workers
                for bg in &plan.batch_groups {
                    let results = &results;
                    let index = &index;
                    let deploy_target = &deploy_targets[bg.target_idx];
                    let target = crate::config::ResolvedTarget {
                        service: bg.target_name.clone(),
                        app: bg.app.clone(),
                        environment: env_name.clone(),
                    };
                    let target_display = target.target_display();

                    s.spawn(move || {
                        let batch_results = deploy_target.deploy_batch(&bg.secrets, &target);

                        let mut idx = index.lock().unwrap();
                        let mut res = results.lock().unwrap();

                        if batch_results.is_empty() {
                            // Tombstone-only batch
                            for key in &bg.tombstoned_keys {
                                let tracker_key = DeployIndex::tracker_key(
                                    key,
                                    &bg.target_name,
                                    bg.app.as_deref(),
                                    env_name,
                                );
                                idx.record_success(
                                    tracker_key,
                                    target.to_string(),
                                    DeployIndex::TOMBSTONE_HASH.to_string(),
                                );
                                if let Some(kr) = res.get_mut(key) {
                                    kr.completed_ops += 1;
                                }
                            }
                        } else {
                            for result in &batch_results {
                                let tracker_key = DeployIndex::tracker_key(
                                    &result.key,
                                    &bg.target_name,
                                    bg.app.as_deref(),
                                    env_name,
                                );
                                let composite = format!("{}:{}", result.key, env_name);
                                let value = payload_secrets
                                    .get(&composite)
                                    .map_or("", std::string::String::as_str);
                                let value_hash = DeployIndex::hash_value(value);

                                if result.outcome.is_success() {
                                    idx.record_success(
                                        tracker_key,
                                        target.to_string(),
                                        value_hash,
                                    );
                                    if let Some(kr) = res.get_mut(&result.key) {
                                        kr.completed_ops += 1;
                                    }
                                } else {
                                    let error = result
                                        .outcome
                                        .error_message()
                                        .unwrap_or_default()
                                        .to_string();
                                    idx.record_failure(
                                        tracker_key,
                                        target.to_string(),
                                        value_hash,
                                        error.clone(),
                                    );
                                    if let Some(kr) = res.get_mut(&result.key) {
                                        kr.completed_ops += 1;
                                        kr.failed
                                            .push((target_display.clone(), error));
                                    }
                                }
                            }
                        }
                        let _ = idx.save();
                    });
                }

                // Individual deploy workers
                for (key, value, target) in &plan.individual {
                    let results = &results;
                    let index = &index;
                    let (target_idx, _) = target_map[target.service.as_str()];
                    let deploy_target = &deploy_targets[target_idx];

                    s.spawn(move || {
                        let result = deploy_target.deploy_secret(key, value, target);
                        let tracker_key = DeployIndex::tracker_key(
                            key,
                            &target.service,
                            target.app.as_deref(),
                            &target.environment,
                        );
                        let value_hash = DeployIndex::hash_value(value);

                        let mut idx = index.lock().unwrap();
                        let mut res = results.lock().unwrap();
                        let target_display = target.target_display();

                        match result {
                            Ok(()) => {
                                idx.record_success(
                                    tracker_key,
                                    target.to_string(),
                                    value_hash,
                                );
                                if let Some(kr) = res.get_mut(key.as_str()) {
                                    kr.completed_ops += 1;
                                }
                            }
                            Err(e) => {
                                idx.record_failure(
                                    tracker_key,
                                    target.to_string(),
                                    value_hash,
                                    e.to_string(),
                                );
                                if let Some(kr) = res.get_mut(key.as_str()) {
                                    kr.completed_ops += 1;
                                    kr.failed.push((target_display, e.to_string()));
                                }
                            }
                        }
                        let _ = idx.save();
                    });
                }

                // Tombstone delete workers
                for (key, target) in &plan.tombstones {
                    let results = &results;
                    let index = &index;
                    let (target_idx, _) = target_map[target.service.as_str()];
                    let deploy_target = &deploy_targets[target_idx];

                    s.spawn(move || {
                        let result = deploy_target.delete_secret(key, target);
                        let tracker_key = DeployIndex::tracker_key(
                            key,
                            &target.service,
                            target.app.as_deref(),
                            &target.environment,
                        );

                        let mut idx = index.lock().unwrap();
                        let mut res = results.lock().unwrap();
                        let target_display = target.target_display();

                        match result {
                            Ok(()) => {
                                idx.record_success(
                                    tracker_key,
                                    target_display,
                                    DeployIndex::TOMBSTONE_HASH.to_string(),
                                );
                                if let Some(kr) = res.get_mut(key.as_str()) {
                                    kr.completed_ops += 1;
                                }
                            }
                            Err(e) => {
                                idx.record_failure(
                                    tracker_key,
                                    target_display.clone(),
                                    DeployIndex::TOMBSTONE_HASH.to_string(),
                                    e.to_string(),
                                );
                                if let Some(kr) = res.get_mut(key.as_str()) {
                                    kr.completed_ops += 1;
                                    kr.failed.push((target_display, e.to_string()));
                                }
                            }
                        }
                        let _ = idx.save();
                    });
                }

                // Batch prune workers
                for ((target_name, app), orphan_list) in &plan.batch_prune {
                    let results = &results;
                    let index = &index;
                    let failed_batch_groups = &failed_batch_groups;
                    let group_key = (target_name.clone(), app.clone(), env_name.clone());

                    s.spawn(move || {
                        let mut idx = index.lock().unwrap();
                        let mut res = results.lock().unwrap();

                        if failed_batch_groups.lock().unwrap().contains(&group_key) {
                            for orphan in orphan_list {
                                if let Some(kr) = res.get_mut(&orphan.key) {
                                    kr.completed_ops += 1;
                                    kr.failed.push((
                                        orphan.target_display(),
                                        "skipped: batch deploy had failures".to_string(),
                                    ));
                                }
                            }
                            return;
                        }

                        for orphan in orphan_list {
                            let (target_idx, _) = target_map[target_name.as_str()];
                            let deploy_target = &deploy_targets[target_idx];
                            let target = crate::config::ResolvedTarget {
                                service: orphan.service.clone(),
                                app: orphan.app.clone(),
                                environment: orphan.env.clone(),
                            };
                            match deploy_target.delete_secret(&orphan.key, &target) {
                                Ok(()) => {
                                    idx.remove_record(&orphan.tracker_key);
                                    if let Some(kr) = res.get_mut(&orphan.key) {
                                        kr.completed_ops += 1;
                                    }
                                }
                                Err(e) => {
                                    if let Some(kr) = res.get_mut(&orphan.key) {
                                        kr.completed_ops += 1;
                                        kr.failed
                                            .push((orphan.target_display(), e.to_string()));
                                    }
                                }
                            }
                        }
                        let _ = idx.save();
                    });
                }

                // Individual prune workers
                for orphan in &plan.prune_individual {
                    let results = &results;
                    let index = &index;
                    let (target_idx, _) = target_map[orphan.service.as_str()];
                    let deploy_target = &deploy_targets[target_idx];
                    let target = crate::config::ResolvedTarget {
                        service: orphan.service.clone(),
                        app: orphan.app.clone(),
                        environment: orphan.env.clone(),
                    };

                    s.spawn(move || {
                        let result = deploy_target.delete_secret(&orphan.key, &target);
                        let mut idx = index.lock().unwrap();
                        let mut res = results.lock().unwrap();
                        let target_display = orphan.target_display();

                        match result {
                            Ok(()) => {
                                idx.remove_record(&orphan.tracker_key);
                                if let Some(kr) = res.get_mut(&orphan.key) {
                                    kr.completed_ops += 1;
                                }
                            }
                            Err(e) => {
                                if let Some(kr) = res.get_mut(&orphan.key) {
                                    kr.completed_ops += 1;
                                    kr.failed.push((target_display, e.to_string()));
                                }
                            }
                        }
                        let _ = idx.save();
                    });
                }

                // Animated render loop on main thread
                let mut frame = 0usize;
                loop {
                    std::thread::sleep(ui::SPINNER_INTERVAL);
                    frame = (frame + 1) % frames.len();

                    let state = results.lock().unwrap();
                    let all_done = key_lines
                        .iter()
                        .all(|kl| kl.total_ops == 0 || state.get(&kl.key).is_none_or(KeyResult::is_done));

                    let _ = term.move_cursor_up(n);
                    for kl in &key_lines {
                        let _ = term.clear_line();
                        if kl.total_ops == 0 {
                            let label =
                                format!("{} {}", ui::icon_unset(), style(&kl.key).dim());
                            let _ = term.write_line(&format!(
                                "{bar}    {}",
                                ui::format_aligned_line(&label, "", label_col)
                            ));
                        } else if let Some(kr) = state.get(&kl.key) {
                            let targets_str = kl.targets.join(", ");
                            if kr.is_done() {
                                let icon = if kr.has_failure() {
                                    ui::icon_failure()
                                } else {
                                    ui::icon_success()
                                };
                                let label = format!("{} {}", icon, style(&kl.key).dim());
                                let _ = term.write_line(&format!(
                                    "{bar}    {}",
                                    ui::format_aligned_line(&label, &targets_str, label_col)
                                ));
                            } else {
                                let label = format!(
                                    "{} {}",
                                    style(frames[frame]).magenta(),
                                    style(&kl.key).dim()
                                );
                                let _ = term.write_line(&format!(
                                    "{bar}    {}",
                                    ui::format_aligned_line(&label, &targets_str, label_col)
                                ));
                            }
                        }
                    }

                    drop(state);
                    if all_done {
                        break;
                    }
                }
            });

            // Collect results into report vectors
            let final_results = results.into_inner().unwrap();
            let mut env_deployed = 0usize;
            let mut env_failed = 0usize;
            let env_unset_count = env_unset.len();
            let mut env_pruned = 0usize;

            for kl in &key_lines {
                if kl.total_ops == 0 {
                    continue; // unset, already counted
                }
                if let Some(kr) = final_results.get(&kl.key) {
                    if kr.has_failure() {
                        for (target_display, error) in &kr.failed {
                            failed.push(DeployEntry {
                                key: kl.key.clone(),
                                env: env_name.clone(),
                                target: target_display.clone(),
                                error: Some(error.clone()),
                            });
                            env_failed += 1;
                        }
                        // Count non-failed ops as deployed
                        let ok_count = kr.completed_ops.saturating_sub(kr.failed.len());
                        for target in kl.targets.iter().take(ok_count) {
                            deployed.push(DeployEntry {
                                key: kl.key.clone(),
                                env: env_name.clone(),
                                target: target.clone(),
                                error: None,
                            });
                            env_deployed += 1;
                        }
                    } else {
                        for target in &kl.targets {
                            deployed.push(DeployEntry {
                                key: kl.key.clone(),
                                env: env_name.clone(),
                                target: target.clone(),
                                error: None,
                            });
                        }
                        env_deployed += kr.completed_ops;
                    }
                }
            }

            // Check if any prune ops happened
            for orphan_list in plan.batch_prune.values() {
                for orphan in orphan_list {
                    if let Some(kr) = final_results.get(&orphan.key) {
                        if !kr.has_failure() {
                            pruned.push(DeployEntry {
                                key: orphan.key.clone(),
                                env: env_name.clone(),
                                target: orphan.target_display(),
                                error: None,
                            });
                            env_pruned += 1;
                        }
                    }
                }
            }
            for orphan in &plan.prune_individual {
                if let Some(kr) = final_results.get(&orphan.key) {
                    if !kr.has_failure() {
                        pruned.push(DeployEntry {
                            key: orphan.key.clone(),
                            env: env_name.clone(),
                            target: orphan.target_display(),
                            error: None,
                        });
                        env_pruned += 1;
                    }
                }
            }

            // Print summary line
            let summary = ui::format_count_summary(&[
                ("failed", env_failed),
                ("deployed", env_deployed),
                ("unset", env_unset_count),
                ("pruned", env_pruned),
            ]);
            let summary_icon = if env_failed > 0 {
                ui::icon_failure()
            } else {
                ui::icon_success()
            };
            let _ = term.write_line(&format!(
                "{}    {} Deployment complete ({})",
                style("\u{2502}").dim(),
                summary_icon,
                summary,
            ));
            let _ = term.write_line(&format!("{}", style("\u{2502}").dim()));
        } else {
            // ---------------------------------------------------------------
            // Sequential mode (verbose / dry_run / non-TTY)
            // ---------------------------------------------------------------

            // Batch groups
            for bg in &plan.batch_groups {
                let deploy_target = &deploy_targets[bg.target_idx];
                let target = crate::config::ResolvedTarget {
                    service: bg.target_name.clone(),
                    app: bg.app.clone(),
                    environment: env_name.clone(),
                };
                let target_display = target.target_display();

                if dry_run {
                    if bg.secrets.is_empty() {
                        for key in &bg.tombstoned_keys {
                            deployed.push(DeployEntry {
                                key: key.clone(),
                                env: env_name.clone(),
                                target: target_display.clone(),
                                error: None,
                            });
                        }
                        continue;
                    }
                    for s in &bg.secrets {
                        deployed.push(DeployEntry {
                            key: s.key.clone(),
                            env: env_name.clone(),
                            target: target_display.clone(),
                            error: None,
                        });
                    }
                    continue;
                }

                if verbose {
                    cliclack::log::step(format!(
                        "Deploying {} ({} secrets) → {}",
                        style(&bg.target_name).bold(),
                        bg.secrets.len(),
                        target
                    ))?;
                }

                let batch_results = deploy_target.deploy_batch(&bg.secrets, &target);
                let mut idx = index.lock().unwrap();

                if batch_results.is_empty() {
                    for key in &bg.tombstoned_keys {
                        let tracker_key = DeployIndex::tracker_key(
                            key,
                            &bg.target_name,
                            bg.app.as_deref(),
                            env_name,
                        );
                        idx.record_success(
                            tracker_key,
                            target.to_string(),
                            DeployIndex::TOMBSTONE_HASH.to_string(),
                        );
                        deployed.push(DeployEntry {
                            key: key.clone(),
                            env: env_name.clone(),
                            target: target_display.clone(),
                            error: None,
                        });
                    }
                    idx.save()?;
                    continue;
                }

                for result in &batch_results {
                    let tracker_key = DeployIndex::tracker_key(
                        &result.key,
                        &bg.target_name,
                        bg.app.as_deref(),
                        env_name,
                    );
                    let composite = format!("{}:{}", result.key, env_name);
                    let value = payload
                        .secrets
                        .get(&composite)
                        .map_or("", std::string::String::as_str);
                    let value_hash = DeployIndex::hash_value(value);

                    if result.outcome.is_success() {
                        idx.record_success(tracker_key, target.to_string(), value_hash);
                        deployed.push(DeployEntry {
                            key: result.key.clone(),
                            env: env_name.clone(),
                            target: target_display.clone(),
                            error: None,
                        });
                    } else {
                        let error = result
                            .outcome
                            .error_message()
                            .unwrap_or_default()
                            .to_string();
                        idx.record_failure(
                            tracker_key,
                            target.to_string(),
                            value_hash,
                            error.clone(),
                        );
                        failed.push(DeployEntry {
                            key: result.key.clone(),
                            env: env_name.clone(),
                            target: target_display.clone(),
                            error: Some(error),
                        });
                        failed_batch_groups.lock().unwrap().insert((
                            bg.target_name.clone(),
                            bg.app.clone(),
                            env_name.clone(),
                        ));
                    }
                }
                idx.save()?;
            }

            // Batch prune
            for ((target_name, app), orphan_list) in &plan.batch_prune {
                let group_key = (target_name.clone(), app.clone(), env_name.clone());
                if failed_batch_groups.lock().unwrap().contains(&group_key) {
                    for orphan in orphan_list {
                        failed.push(DeployEntry {
                            key: orphan.key.clone(),
                            env: env_name.clone(),
                            target: orphan.target_display(),
                            error: Some("skipped: batch deploy had failures".to_string()),
                        });
                    }
                    continue;
                }
                let mut idx = index.lock().unwrap();
                for orphan in orphan_list {
                    let target_display = orphan.target_display();
                    if dry_run {
                        pruned.push(DeployEntry {
                            key: orphan.key.clone(),
                            env: env_name.clone(),
                            target: target_display,
                            error: None,
                        });
                    } else {
                        let (target_idx, _) = target_map[target_name.as_str()];
                        let deploy_target = &deploy_targets[target_idx];
                        let target = crate::config::ResolvedTarget {
                            service: orphan.service.clone(),
                            app: orphan.app.clone(),
                            environment: orphan.env.clone(),
                        };
                        match deploy_target.delete_secret(&orphan.key, &target) {
                            Ok(()) => {
                                idx.remove_record(&orphan.tracker_key);
                                pruned.push(DeployEntry {
                                    key: orphan.key.clone(),
                                    env: env_name.clone(),
                                    target: target_display,
                                    error: None,
                                });
                            }
                            Err(e) => {
                                failed.push(DeployEntry {
                                    key: orphan.key.clone(),
                                    env: env_name.clone(),
                                    target: target_display,
                                    error: Some(e.to_string()),
                                });
                            }
                        }
                    }
                }
                if !dry_run {
                    idx.save()?;
                }
            }

            // Individual deploys
            for (key, value, target) in &plan.individual {
                let target_display = target.target_display();

                if dry_run {
                    deployed.push(DeployEntry {
                        key: key.clone(),
                        env: env_name.clone(),
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

                let mut idx = index.lock().unwrap();
                match result {
                    Ok(()) => {
                        idx.record_success(tracker_key, target.to_string(), value_hash);
                        deployed.push(DeployEntry {
                            key: key.clone(),
                            env: env_name.clone(),
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
                        idx.record_failure(
                            tracker_key,
                            target.to_string(),
                            value_hash,
                            e.to_string(),
                        );
                        failed.push(DeployEntry {
                            key: key.clone(),
                            env: env_name.clone(),
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
                idx.save()?;
            }

            // Tombstone deletes
            for (key, target) in &plan.tombstones {
                let target_display = target.target_display();

                if dry_run {
                    deployed.push(DeployEntry {
                        key: key.clone(),
                        env: env_name.clone(),
                        target: target_display,
                        error: None,
                    });
                    continue;
                }

                let (target_idx, _) = target_map[target.service.as_str()];
                let deploy_target = &deploy_targets[target_idx];

                let mut idx = index.lock().unwrap();
                match deploy_target.delete_secret(key, target) {
                    Ok(()) => {
                        let tracker_key = DeployIndex::tracker_key(
                            key,
                            &target.service,
                            target.app.as_deref(),
                            &target.environment,
                        );
                        idx.record_success(
                            tracker_key,
                            target_display,
                            DeployIndex::TOMBSTONE_HASH.to_string(),
                        );
                        deployed.push(DeployEntry {
                            key: key.clone(),
                            env: env_name.clone(),
                            target: target.target_display(),
                            error: None,
                        });
                    }
                    Err(e) => {
                        let tracker_key = DeployIndex::tracker_key(
                            key,
                            &target.service,
                            target.app.as_deref(),
                            &target.environment,
                        );
                        idx.record_failure(
                            tracker_key,
                            target_display,
                            DeployIndex::TOMBSTONE_HASH.to_string(),
                            e.to_string(),
                        );
                        failed.push(DeployEntry {
                            key: key.clone(),
                            env: env_name.clone(),
                            target: target.target_display(),
                            error: Some(e.to_string()),
                        });
                    }
                }
                idx.save()?;
            }

            // Individual prune
            for orphan in &plan.prune_individual {
                let target_display = orphan.target_display();
                let target = crate::config::ResolvedTarget {
                    service: orphan.service.clone(),
                    app: orphan.app.clone(),
                    environment: orphan.env.clone(),
                };

                if dry_run {
                    pruned.push(DeployEntry {
                        key: orphan.key.clone(),
                        env: env_name.clone(),
                        target: target_display,
                        error: None,
                    });
                    continue;
                }

                if verbose {
                    cliclack::log::step(format!(
                        "Pruning {}:{} → {}",
                        orphan.key, orphan.env, target
                    ))?;
                }

                let (target_idx, _) = target_map[orphan.service.as_str()];
                let deploy_target = &deploy_targets[target_idx];

                let mut idx = index.lock().unwrap();
                match deploy_target.delete_secret(&orphan.key, &target) {
                    Ok(()) => {
                        idx.remove_record(&orphan.tracker_key);
                        pruned.push(DeployEntry {
                            key: orphan.key.clone(),
                            env: env_name.clone(),
                            target: target_display,
                            error: None,
                        });
                        idx.save()?;
                    }
                    Err(e) => {
                        failed.push(DeployEntry {
                            key: orphan.key.clone(),
                            env: env_name.clone(),
                            target: target_display,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }
        }
    }

    // Warn about orphans whose target is no longer configured
    if !unavailable_orphans.is_empty() {
        let lines: Vec<String> = unavailable_orphans
            .iter()
            .map(|o| format!("  {} → {} ({})", o.key, o.target_display(), o.env))
            .collect();
        cliclack::log::warning(format!(
            "Cannot prune — target no longer configured:\n{}\n  \
             Remove these manually or re-add the target config.",
            lines.join("\n")
        ))?;
    }

    if !dry_run {
        index.lock().unwrap().save()?;
    }

    let report = DeployReport {
        deployed,
        failed,
        skipped,
        unset,
        pruned,
        dry_run,
        verbose,
    };

    // In animated mode, results were shown live — only show skipped + dry_run notice
    if animated {
        if !report.skipped.is_empty() {
            if report.verbose {
                // (verbose is false when animated, so this branch is unreachable,
                //  but kept for consistency)
                report.render_skipped()?;
            } else {
                let skip_count = report.skipped.len();
                cliclack::log::remark(format!(
                    "{} targets up to date  {}",
                    style(skip_count).bold(),
                    style("(use --verbose to show)").dim()
                ))?;
            }
        }

        if report.is_empty() && !report.dry_run {
            cliclack::log::info("Nothing to deploy.")?;
        }

        if report.dry_run {
            cliclack::log::warning("Dry run — no changes made".to_string())?;
        }
    } else {
        report.render()?;
    }

    if report.has_failures() {
        anyhow::bail!("{} deploy(s) failed", report.failed.len());
    }

    Ok(())
}
