use anyhow::Result;
use console::style;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::IsTerminal;
use std::sync::Mutex;

use crate::deploy_tracker::DeployIndex;
use crate::targets::{DeployMode, DeployTarget};
use crate::ui;

use super::report::{DeployEntry, DeployReport};
use super::types::{EnvWorkPlan, KeyLine, KeyResult, PlanOutput, DEPLOY_LINE_WIDTH};
use super::DeployOptions;

pub(crate) fn execute_deploy<'a>(
    plan: &PlanOutput,
    deploy_targets: &[Box<dyn DeployTarget + 'a>],
    target_map: &HashMap<&str, (usize, DeployMode)>,
    payload_secrets: &BTreeMap<String, String>,
    master_key: &[u8],
    index: &Mutex<DeployIndex>,
    opts: &DeployOptions<'_>,
) -> Result<DeployReport> {
    let DeployOptions {
        dry_run, verbose, ..
    } = *opts;

    let is_tty = std::io::stderr().is_terminal();
    let animated = !verbose && !dry_run && is_tty;

    let mut deployed: Vec<DeployEntry> = Vec::new();
    let mut failed: Vec<DeployEntry> = Vec::new();
    let mut pruned: Vec<DeployEntry> = Vec::new();

    // Batch groups that had at least one failure (skip pruning for these)
    let failed_batch_groups: Mutex<BTreeSet<(String, Option<String>, String)>> =
        Mutex::new(BTreeSet::new());

    for (env_name, env_plan) in &plan.env_plans {
        // Group unset entries for this env
        let env_unset: Vec<&DeployEntry> =
            plan.unset.iter().filter(|e| e.env == *env_name).collect();

        let key_lines = build_key_lines(env_plan, &env_unset);
        let has_work = env_plan.has_work();

        if !has_work && env_unset.is_empty() {
            continue;
        }

        // Compute label column for dot-alignment
        let max_key_len = key_lines.iter().map(|kl| kl.key.len()).max().unwrap_or(0);
        let label_col = DEPLOY_LINE_WIDTH.max(max_key_len + 7);

        if animated && has_work {
            execute_animated(
                env_name,
                env_plan,
                &key_lines,
                &env_unset,
                label_col,
                deploy_targets,
                target_map,
                payload_secrets,
                master_key,
                index,
                &failed_batch_groups,
                &mut deployed,
                &mut failed,
                &mut pruned,
            );
        } else {
            execute_sequential(
                env_name,
                env_plan,
                deploy_targets,
                target_map,
                payload_secrets,
                master_key,
                index,
                &failed_batch_groups,
                &mut deployed,
                &mut failed,
                &mut pruned,
                dry_run,
                verbose,
            )?;
        }
    }

    Ok(DeployReport {
        deployed,
        failed,
        skipped: Vec::new(),
        unset: Vec::new(),
        pruned,
        unavailable_orphans: Vec::new(),
        restored: Vec::new(),
        dry_run,
        verbose,
    })
}

/// Build the final report by merging execution results with plan outputs.
pub(crate) fn build_report(mut exec_report: DeployReport, plan: PlanOutput) -> DeployReport {
    exec_report.skipped = plan.skipped;
    exec_report.unset = plan.unset;
    exec_report.unavailable_orphans = plan.unavailable_orphans;
    exec_report.restored = plan.restored;
    exec_report
}

/// Render post-execution output depending on animated vs sequential mode.
pub(crate) fn render_report(report: &DeployReport, animated: bool) -> Result<()> {
    if animated {
        report.render_restored()?;
        if !report.skipped.is_empty() {
            if report.verbose {
                report.render_skipped()?;
            } else {
                let skip_count = report.skipped.len();
                cliclack::log::remark(format!(
                    "{} targets unchanged since last deploy  {}",
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
    Ok(())
}

// -----------------------------------------------------------------------
// Shared deploy/delete + index recording helpers
// -----------------------------------------------------------------------

struct BatchExecResult {
    /// Per-key outcomes: (key, error_if_failed).
    items: Vec<(String, Option<String>)>,
    had_failure: bool,
}

/// Deploy a batch group and record results in the index.
fn exec_batch_group(
    bg: &super::types::BatchGroup,
    env_name: &str,
    deploy_target: &dyn DeployTarget,
    payload_secrets: &BTreeMap<String, String>,
    master_key: &[u8],
    index: &Mutex<DeployIndex>,
) -> BatchExecResult {
    let target = crate::config::ResolvedTarget {
        service: bg.target_name.clone(),
        app: bg.app.clone(),
        environment: env_name.to_string(),
    };

    let mut items = Vec::new();
    let mut had_failure = false;

    let expected_keys: BTreeSet<&str> = bg
        .secrets
        .iter()
        .map(|secret| secret.key.as_str())
        .collect();
    let batch_deploy = deploy_target
        .deploy_batch_state(
            crate::targets::BatchDeployment {
                secrets: &bg.secrets,
                tombstoned_keys: &bg.tombstoned_keys,
            },
            &target,
        )
        .and_then(|results| {
            let actual_keys: BTreeSet<&str> =
                results.iter().map(|result| result.key.as_str()).collect();
            if results.len() != expected_keys.len() || actual_keys != expected_keys {
                anyhow::bail!("batch target returned an invalid per-secret result set");
            }
            Ok(results)
        });

    let batch_results = match batch_deploy {
        Ok(results) => results,
        Err(error) => {
            let error = crate::targets::redact_secrets(
                &error.to_string(),
                bg.secrets.iter().map(|secret| secret.value.as_str()),
            );
            let mut idx = index.lock().expect("deploy index mutex poisoned");
            for secret in &bg.secrets {
                let tracker_key = DeployIndex::tracker_key(
                    &secret.key,
                    &bg.target_name,
                    bg.app.as_deref(),
                    env_name,
                );
                let value_hash = DeployIndex::hash_value(&secret.value, master_key);
                idx.record_failure(tracker_key, target.to_string(), value_hash, error.clone());
                items.push((secret.key.clone(), Some(error.clone())));
            }
            for key in &bg.tombstoned_keys {
                let tracker_key =
                    DeployIndex::tracker_key(key, &bg.target_name, bg.app.as_deref(), env_name);
                idx.record_failure(
                    tracker_key,
                    target.to_string(),
                    DeployIndex::TOMBSTONE_HASH.to_string(),
                    error.clone(),
                );
                items.push((key.clone(), Some(error.clone())));
            }
            return BatchExecResult {
                items,
                had_failure: true,
            };
        }
    };

    let mut idx = index.lock().expect("deploy index mutex poisoned");

    if batch_results.is_empty() {
        // Tombstone-only batch
        for key in &bg.tombstoned_keys {
            let tracker_key =
                DeployIndex::tracker_key(key, &bg.target_name, bg.app.as_deref(), env_name);
            idx.record_success(
                tracker_key,
                target.to_string(),
                DeployIndex::TOMBSTONE_HASH.to_string(),
            );
            items.push((key.clone(), None));
        }
    } else {
        for result in &batch_results {
            let tracker_key =
                DeployIndex::tracker_key(&result.key, &bg.target_name, bg.app.as_deref(), env_name);
            let composite = format!("{}:{}", result.key, env_name);
            let value = payload_secrets
                .get(&composite)
                .map_or("", std::string::String::as_str);
            let value_hash = DeployIndex::hash_value(value, master_key);

            if result.outcome.is_success() {
                idx.record_success(tracker_key, target.to_string(), value_hash);
                items.push((result.key.clone(), None));
            } else {
                had_failure = true;
                let error = result
                    .outcome
                    .error_message()
                    .unwrap_or_default()
                    .to_string();
                let error = crate::targets::redact_secrets(
                    &error,
                    bg.secrets.iter().map(|secret| secret.value.as_str()),
                );
                idx.record_failure(tracker_key, target.to_string(), value_hash, error.clone());
                items.push((result.key.clone(), Some(error)));
            }
        }

        for key in &bg.tombstoned_keys {
            let tracker_key =
                DeployIndex::tracker_key(key, &bg.target_name, bg.app.as_deref(), env_name);
            if had_failure {
                let error = "batch deploy had failures".to_string();
                idx.record_failure(
                    tracker_key,
                    target.to_string(),
                    DeployIndex::TOMBSTONE_HASH.to_string(),
                    error.clone(),
                );
                items.push((key.clone(), Some(error)));
            } else {
                idx.record_success(
                    tracker_key,
                    target.to_string(),
                    DeployIndex::TOMBSTONE_HASH.to_string(),
                );
                items.push((key.clone(), None));
            }
        }
    }

    BatchExecResult { items, had_failure }
}

/// Deploy a single secret and record the result in the index.
/// Returns `Ok(())` on success, `Err(error_message)` on failure.
fn exec_individual_deploy(
    key: &str,
    value: &str,
    target: &crate::config::ResolvedTarget,
    deploy_target: &dyn DeployTarget,
    master_key: &[u8],
    index: &Mutex<DeployIndex>,
) -> Result<(), String> {
    let result = deploy_target.deploy_secret(key, value, target);

    let tracker_key = DeployIndex::tracker_key(
        key,
        &target.service,
        target.app.as_deref(),
        &target.environment,
    );
    let value_hash = DeployIndex::hash_value(value, master_key);

    let mut idx = index.lock().expect("deploy index mutex poisoned");
    match result {
        Ok(()) => {
            idx.record_success(tracker_key, target.to_string(), value_hash);
            Ok(())
        }
        Err(e) => {
            let msg = crate::targets::redact_secrets(&e.to_string(), [value]);
            idx.record_failure(tracker_key, target.to_string(), value_hash, msg.clone());
            Err(msg)
        }
    }
}

/// Delete a tombstoned secret and record the result in the index.
/// Returns `Ok(())` on success, `Err(error_message)` on failure.
fn exec_tombstone_delete(
    key: &str,
    target: &crate::config::ResolvedTarget,
    deploy_target: &dyn DeployTarget,
    index: &Mutex<DeployIndex>,
) -> Result<(), String> {
    let result = deploy_target.delete_secret(key, target);

    let tracker_key = DeployIndex::tracker_key(
        key,
        &target.service,
        target.app.as_deref(),
        &target.environment,
    );

    let mut idx = index.lock().expect("deploy index mutex poisoned");
    match result {
        Ok(()) => {
            idx.record_success(
                tracker_key,
                target.target_display(),
                DeployIndex::TOMBSTONE_HASH.to_string(),
            );
            Ok(())
        }
        Err(e) => {
            let msg = e.to_string();
            idx.record_failure(
                tracker_key,
                target.target_display(),
                DeployIndex::TOMBSTONE_HASH.to_string(),
                msg.clone(),
            );
            Err(msg)
        }
    }
}

/// Delete a pruned orphan and remove its record from the index.
/// Returns `Ok(())` on success, `Err(error_message)` on failure.
fn exec_prune_orphan(
    orphan: &crate::orphan::TargetOrphan,
    deploy_target: &dyn DeployTarget,
    index: &Mutex<DeployIndex>,
) -> Result<(), String> {
    let target = crate::config::ResolvedTarget {
        service: orphan.service.clone(),
        app: orphan.app.clone(),
        environment: orphan.env.clone(),
    };

    let result = deploy_target.delete_secret(&orphan.key, &target);

    match result {
        Ok(()) => {
            let mut idx = index.lock().expect("deploy index mutex poisoned");
            idx.remove_record(&orphan.tracker_key);
            Ok(())
        }
        Err(e) => Err(e.to_string()),
    }
}

// -----------------------------------------------------------------------
// Animated execution
// -----------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_animated<'a>(
    env_name: &str,
    plan: &EnvWorkPlan,
    key_lines: &[KeyLine],
    env_unset: &[&DeployEntry],
    label_col: usize,
    deploy_targets: &[Box<dyn DeployTarget + 'a>],
    target_map: &HashMap<&str, (usize, DeployMode)>,
    payload_secrets: &BTreeMap<String, String>,
    master_key: &[u8],
    index: &Mutex<DeployIndex>,
    failed_batch_groups: &Mutex<BTreeSet<(String, Option<String>, String)>>,
    deployed: &mut Vec<DeployEntry>,
    failed: &mut Vec<DeployEntry>,
    pruned: &mut Vec<DeployEntry>,
) {
    let n = key_lines.len();
    let results: Mutex<BTreeMap<String, KeyResult>> = Mutex::new(BTreeMap::new());

    // Initialize results
    {
        let mut r = results.lock().expect("results mutex poisoned");
        for kl in key_lines {
            r.insert(
                kl.key.clone(),
                KeyResult {
                    completed_ops: 0,
                    total_ops: kl.total_ops,
                    succeeded: Vec::new(),
                    failed: Vec::new(),
                },
            );
        }
    }

    let term = console::Term::stderr();
    let frames = ui::SPINNER_FRAMES;
    let bar = style("\u{2502}").dim();

    // Print header + initial spinner lines
    let _ = term.write_line(&format!("{}  {}", style("\u{25C7}").dim(), env_name));
    for kl in key_lines {
        if kl.total_ops == 0 {
            // Unset key — show immediately
            let label = format!("{} {}", ui::Icon::Unset, style(&kl.key).dim());
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
            let deploy_target = &deploy_targets[bg.target_idx];
            let target_display =
                crate::config::format_target_label(&bg.target_name, bg.app.as_deref());

            s.spawn(move || {
                let outcome = exec_batch_group(
                    bg,
                    env_name,
                    deploy_target.as_ref(),
                    payload_secrets,
                    master_key,
                    index,
                );

                let mut res = results.lock().expect("results mutex poisoned");
                for (key, error) in &outcome.items {
                    if let Some(kr) = res.get_mut(key.as_str()) {
                        kr.completed_ops += 1;
                        if let Some(e) = error {
                            kr.failed.push((target_display.clone(), e.clone()));
                        } else {
                            kr.succeeded.push(target_display.clone());
                        }
                    }
                }

                if outcome.had_failure {
                    failed_batch_groups
                        .lock()
                        .expect("failed batch groups mutex poisoned")
                        .insert((bg.target_name.clone(), bg.app.clone(), env_name.to_string()));
                }
            });
        }

        // Individual deploy workers
        for (key, value, target) in &plan.individual {
            let results = &results;
            let (target_idx, _) = target_map[target.service.as_str()];
            let deploy_target = &deploy_targets[target_idx];

            s.spawn(move || {
                let outcome = exec_individual_deploy(
                    key,
                    value,
                    target,
                    deploy_target.as_ref(),
                    master_key,
                    index,
                );
                let mut res = results.lock().expect("results mutex poisoned");
                if let Some(kr) = res.get_mut(key.as_str()) {
                    kr.completed_ops += 1;
                    if let Err(e) = outcome {
                        kr.failed.push((target.target_display(), e));
                    } else {
                        kr.succeeded.push(target.target_display());
                    }
                }
            });
        }

        // Tombstone delete workers
        for (key, target) in &plan.tombstones {
            let results = &results;
            let (target_idx, _) = target_map[target.service.as_str()];
            let deploy_target = &deploy_targets[target_idx];

            s.spawn(move || {
                let outcome = exec_tombstone_delete(key, target, deploy_target.as_ref(), index);
                let mut res = results.lock().expect("results mutex poisoned");
                if let Some(kr) = res.get_mut(key.as_str()) {
                    kr.completed_ops += 1;
                    if let Err(e) = outcome {
                        kr.failed.push((target.target_display(), e));
                    } else {
                        kr.succeeded.push(target.target_display());
                    }
                }
            });
        }

        // Batch prune workers
        for ((target_name, app), orphan_list) in &plan.batch_prune {
            let results = &results;
            let group_key = (target_name.clone(), app.clone(), env_name.to_string());

            s.spawn(move || {
                if failed_batch_groups
                    .lock()
                    .expect("failed batch groups mutex poisoned")
                    .contains(&group_key)
                {
                    let mut res = results.lock().expect("results mutex poisoned");
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
                    let outcome = exec_prune_orphan(orphan, deploy_target.as_ref(), index);
                    let mut res = results.lock().expect("results mutex poisoned");
                    if let Some(kr) = res.get_mut(&orphan.key) {
                        kr.completed_ops += 1;
                        if let Err(e) = outcome {
                            kr.failed.push((orphan.target_display(), e));
                        }
                    }
                }
            });
        }

        // Individual prune workers
        for orphan in &plan.prune_individual {
            let results = &results;
            let (target_idx, _) = target_map[orphan.service.as_str()];
            let deploy_target = &deploy_targets[target_idx];

            s.spawn(move || {
                let outcome = exec_prune_orphan(orphan, deploy_target.as_ref(), index);
                let mut res = results.lock().expect("results mutex poisoned");
                if let Some(kr) = res.get_mut(&orphan.key) {
                    kr.completed_ops += 1;
                    if let Err(e) = outcome {
                        kr.failed.push((orphan.target_display(), e));
                    }
                }
            });
        }

        // Animated render loop on main thread
        let mut frame = 0usize;
        loop {
            std::thread::sleep(ui::SPINNER_INTERVAL);
            frame = (frame + 1) % frames.len();

            let state = results.lock().expect("results mutex poisoned");
            let all_done = key_lines
                .iter()
                .all(|kl| kl.total_ops == 0 || state.get(&kl.key).is_none_or(KeyResult::is_done));

            let _ = term.move_cursor_up(n);
            for kl in key_lines {
                let _ = term.clear_line();
                if kl.total_ops == 0 {
                    let label = format!("{} {}", ui::Icon::Unset, style(&kl.key).dim());
                    let _ = term.write_line(&format!(
                        "{bar}    {}",
                        ui::format_aligned_line(&label, "", label_col)
                    ));
                } else if let Some(kr) = state.get(&kl.key) {
                    let targets_str = kl.targets.join(", ");
                    if kr.is_done() {
                        let icon = if kr.has_failure() {
                            ui::Icon::Failure
                        } else {
                            ui::Icon::Success
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
    let final_results = results.into_inner().expect("results mutex poisoned");
    let mut env_deployed = 0usize;
    let mut env_failed = 0usize;
    let env_unset_count = env_unset.len();
    let mut env_pruned = 0usize;

    for kl in key_lines {
        if kl.total_ops == 0 {
            continue; // unset, already counted
        }
        if let Some(kr) = final_results.get(&kl.key) {
            if kr.has_failure() {
                for (target_display, error) in &kr.failed {
                    failed.push(DeployEntry {
                        key: kl.key.clone(),
                        env: env_name.to_string(),
                        target: target_display.clone(),
                        error: Some(error.clone()),
                    });
                    env_failed += 1;
                }
                // Preserve the target attached to each outcome. Worker completion
                // order is intentionally nondeterministic, so inferring success
                // from the first N target labels can attribute a success to the
                // wrong target in a mixed-outcome run.
                let mut succeeded = kr.succeeded.clone();
                succeeded.sort();
                for target in succeeded {
                    deployed.push(DeployEntry {
                        key: kl.key.clone(),
                        env: env_name.to_string(),
                        target,
                        error: None,
                    });
                    env_deployed += 1;
                }
            } else {
                for target in &kl.targets {
                    deployed.push(DeployEntry {
                        key: kl.key.clone(),
                        env: env_name.to_string(),
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
                        env: env_name.to_string(),
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
                    env: env_name.to_string(),
                    target: orphan.target_display(),
                    error: None,
                });
                env_pruned += 1;
            }
        }
    }

    // Repaint header with status color
    let header_icon = if env_failed > 0 && env_deployed == 0 {
        style("\u{25C6}").red()
    } else if env_failed > 0 {
        style("\u{25C6}").yellow()
    } else {
        style("\u{25C6}").green()
    };
    let _ = term.move_cursor_up(n + 1);
    let _ = term.clear_line();
    let _ = term.write_line(&format!("{header_icon}  {env_name}"));
    let _ = term.move_cursor_down(n);

    // Print summary line
    let env_keys = key_lines.iter().filter(|kl| kl.total_ops > 0).count();
    // The animated renderer never runs for a dry run (see `execute_deploy`),
    // so this summary always describes work that happened.
    let summary = ui::format_deploy_summary(
        env_keys,
        env_deployed,
        env_failed,
        env_unset_count,
        env_pruned,
        ui::SummaryMood::Done,
    );
    let summary_icon = if env_failed > 0 {
        ui::Icon::Failure.to_string()
    } else {
        ui::Icon::Pending.color(ui::SectionColor::Green)
    };
    let _ = term.write_line(&format!(
        "{}    {} {}",
        style("\u{2502}").dim(),
        summary_icon,
        summary,
    ));
    let _ = term.write_line(&format!("{}", style("\u{2502}").dim()));
}

// -----------------------------------------------------------------------
// Sequential execution (verbose / dry_run / non-TTY)
// -----------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn execute_sequential<'a>(
    env_name: &str,
    plan: &EnvWorkPlan,
    deploy_targets: &[Box<dyn DeployTarget + 'a>],
    target_map: &HashMap<&str, (usize, DeployMode)>,
    payload_secrets: &BTreeMap<String, String>,
    master_key: &[u8],
    index: &Mutex<DeployIndex>,
    failed_batch_groups: &Mutex<BTreeSet<(String, Option<String>, String)>>,
    deployed: &mut Vec<DeployEntry>,
    failed: &mut Vec<DeployEntry>,
    pruned: &mut Vec<DeployEntry>,
    dry_run: bool,
    verbose: bool,
) -> Result<()> {
    // Batch groups
    for bg in &plan.batch_groups {
        let target_display = crate::config::format_target_label(&bg.target_name, bg.app.as_deref());

        if dry_run {
            if bg.secrets.is_empty() {
                for key in &bg.tombstoned_keys {
                    deployed.push(DeployEntry {
                        key: key.clone(),
                        env: env_name.to_string(),
                        target: target_display.clone(),
                        error: None,
                    });
                }
            } else {
                for s in &bg.secrets {
                    deployed.push(DeployEntry {
                        key: s.key.clone(),
                        env: env_name.to_string(),
                        target: target_display.clone(),
                        error: None,
                    });
                }
            }
            continue;
        }

        if verbose {
            cliclack::log::step(format!(
                "Deploying {} ({} secrets) → {}",
                style(&bg.target_name).bold(),
                bg.secrets.len(),
                target_display
            ))?;
        }

        let deploy_target = &deploy_targets[bg.target_idx];
        let result = exec_batch_group(
            bg,
            env_name,
            deploy_target.as_ref(),
            payload_secrets,
            master_key,
            index,
        );

        for (key, error) in &result.items {
            if let Some(e) = error {
                failed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
                    target: target_display.clone(),
                    error: Some(e.clone()),
                });
            } else {
                deployed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
                    target: target_display.clone(),
                    error: None,
                });
            }
        }

        if result.had_failure {
            failed_batch_groups
                .lock()
                .expect("failed batch groups mutex poisoned")
                .insert((bg.target_name.clone(), bg.app.clone(), env_name.to_string()));
        }

        index.lock().expect("deploy index mutex poisoned").save()?;
    }

    // Batch prune
    for ((target_name, app), orphan_list) in &plan.batch_prune {
        let group_key = (target_name.clone(), app.clone(), env_name.to_string());
        if failed_batch_groups
            .lock()
            .expect("failed batch groups mutex poisoned")
            .contains(&group_key)
        {
            for orphan in orphan_list {
                failed.push(DeployEntry {
                    key: orphan.key.clone(),
                    env: env_name.to_string(),
                    target: orphan.target_display(),
                    error: Some("skipped: batch deploy had failures".to_string()),
                });
            }
            continue;
        }

        for orphan in orphan_list {
            let target_display = orphan.target_display();
            if dry_run {
                pruned.push(DeployEntry {
                    key: orphan.key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: None,
                });
            } else {
                let (target_idx, _) = target_map[target_name.as_str()];
                let deploy_target = &deploy_targets[target_idx];
                match exec_prune_orphan(orphan, deploy_target.as_ref(), index) {
                    Ok(()) => {
                        pruned.push(DeployEntry {
                            key: orphan.key.clone(),
                            env: env_name.to_string(),
                            target: target_display,
                            error: None,
                        });
                    }
                    Err(e) => {
                        failed.push(DeployEntry {
                            key: orphan.key.clone(),
                            env: env_name.to_string(),
                            target: target_display,
                            error: Some(e),
                        });
                    }
                }
            }
        }
        if !dry_run {
            index.lock().expect("deploy index mutex poisoned").save()?;
        }
    }

    // Individual deploys
    for (key, value, target) in &plan.individual {
        let target_display = target.target_display();

        if dry_run {
            deployed.push(DeployEntry {
                key: key.clone(),
                env: env_name.to_string(),
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

        match exec_individual_deploy(
            key,
            value,
            target,
            deploy_target.as_ref(),
            master_key,
            index,
        ) {
            Ok(()) => {
                deployed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
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
                failed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: Some(e.clone()),
                });
                if verbose {
                    let _ = cliclack::log::error(format!(
                        "{}:{} → {}: {}",
                        key, target.environment, target, e
                    ));
                }
            }
        }
        index.lock().expect("deploy index mutex poisoned").save()?;
    }

    // Tombstone deletes
    for (key, target) in &plan.tombstones {
        let target_display = target.target_display();

        if dry_run {
            deployed.push(DeployEntry {
                key: key.clone(),
                env: env_name.to_string(),
                target: target_display,
                error: None,
            });
            continue;
        }

        let (target_idx, _) = target_map[target.service.as_str()];
        let deploy_target = &deploy_targets[target_idx];

        match exec_tombstone_delete(key, target, deploy_target.as_ref(), index) {
            Ok(()) => {
                deployed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: None,
                });
            }
            Err(e) => {
                failed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: Some(e),
                });
            }
        }
        index.lock().expect("deploy index mutex poisoned").save()?;
    }

    // Individual prune
    for orphan in &plan.prune_individual {
        let target_display = orphan.target_display();

        if dry_run {
            pruned.push(DeployEntry {
                key: orphan.key.clone(),
                env: env_name.to_string(),
                target: target_display,
                error: None,
            });
            continue;
        }

        if verbose {
            let target = crate::config::ResolvedTarget {
                service: orphan.service.clone(),
                app: orphan.app.clone(),
                environment: orphan.env.clone(),
            };
            cliclack::log::step(format!(
                "Pruning {}:{} → {}",
                orphan.key, orphan.env, target
            ))?;
        }

        let (target_idx, _) = target_map[orphan.service.as_str()];
        let deploy_target = &deploy_targets[target_idx];

        match exec_prune_orphan(orphan, deploy_target.as_ref(), index) {
            Ok(()) => {
                pruned.push(DeployEntry {
                    key: orphan.key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: None,
                });
                index.lock().expect("deploy index mutex poisoned").save()?;
            }
            Err(e) => {
                failed.push(DeployEntry {
                    key: orphan.key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: Some(e),
                });
            }
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------
// Key line building
// -----------------------------------------------------------------------

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    struct ParallelTarget {
        name: &'static str,
        started: Arc<AtomicUsize>,
        succeeds: bool,
    }

    struct FailingEmptyBatchTarget;

    struct SiblingLeakBatchTarget;

    impl crate::targets::DeployTarget for SiblingLeakBatchTarget {
        fn name(&self) -> &'static str {
            "batch"
        }

        fn deploy_mode(&self) -> crate::targets::DeployMode {
            crate::targets::DeployMode::Batch
        }

        fn deploy_secret(
            &self,
            _key: &str,
            _value: &str,
            _target: &crate::config::ResolvedTarget,
        ) -> anyhow::Result<()> {
            anyhow::bail!("individual deployment is unsupported")
        }

        fn deploy_batch_state(
            &self,
            batch: crate::targets::BatchDeployment<'_>,
            _target: &crate::config::ResolvedTarget,
        ) -> anyhow::Result<Vec<crate::targets::DeployResult>> {
            Ok(batch
                .secrets
                .iter()
                .map(|secret| crate::targets::DeployResult {
                    key: secret.key.clone(),
                    outcome: crate::targets::DeployOutcome::Failed(
                        "provider echoed short-secret and short-secret-suffix".to_string(),
                    ),
                })
                .collect())
        }
    }

    impl crate::targets::DeployTarget for FailingEmptyBatchTarget {
        fn name(&self) -> &'static str {
            ".env"
        }

        fn deploy_mode(&self) -> crate::targets::DeployMode {
            crate::targets::DeployMode::Batch
        }

        fn deploy_secret(
            &self,
            _key: &str,
            _value: &str,
            _target: &crate::config::ResolvedTarget,
        ) -> anyhow::Result<()> {
            anyhow::bail!("individual deployment is unsupported")
        }

        fn deploy_batch_state(
            &self,
            _batch: crate::targets::BatchDeployment<'_>,
            _target: &crate::config::ResolvedTarget,
        ) -> anyhow::Result<Vec<crate::targets::DeployResult>> {
            anyhow::bail!("final-secret write failed")
        }
    }

    #[test]
    fn empty_batch_failure_does_not_acknowledge_final_secret_tombstone() {
        let bg = super::super::types::BatchGroup {
            target_name: ".env".to_string(),
            app: Some("web".to_string()),
            secrets: Vec::new(),
            tombstoned_keys: BTreeSet::from(["LAST_SECRET".to_string()]),
            target_idx: 0,
        };
        let dir = tempfile::tempdir().unwrap();
        let index = Mutex::new(DeployIndex::new(&dir.path().join("deploy-index.json")));

        let outcome = exec_batch_group(
            &bg,
            "dev",
            &FailingEmptyBatchTarget,
            &BTreeMap::new(),
            b"test-master-key",
            &index,
        );

        assert!(outcome.had_failure);
        assert_eq!(
            outcome.items,
            vec![(
                "LAST_SECRET".to_string(),
                Some("final-secret write failed".to_string())
            )]
        );
        let tracker_key = DeployIndex::tracker_key("LAST_SECRET", ".env", Some("web"), "dev");
        let record = &index.lock().unwrap().records[&tracker_key];
        assert_eq!(
            record.last_deploy_status,
            crate::deploy_tracker::DeployStatus::Failed
        );
        assert_eq!(record.value_hash, DeployIndex::TOMBSTONE_HASH);
    }

    #[test]
    fn batch_failure_redacts_all_sibling_values_before_index_and_report() {
        let bg = super::super::types::BatchGroup {
            target_name: "batch".to_string(),
            app: None,
            secrets: vec![
                crate::targets::SecretValue {
                    key: "SHORT".to_string(),
                    value: zeroize::Zeroizing::new("short-secret".to_string()),
                    group: "General".to_string(),
                },
                crate::targets::SecretValue {
                    key: "LONG".to_string(),
                    value: zeroize::Zeroizing::new("short-secret-suffix".to_string()),
                    group: "General".to_string(),
                },
            ],
            tombstoned_keys: BTreeSet::new(),
            target_idx: 0,
        };
        let dir = tempfile::tempdir().unwrap();
        let index = Mutex::new(DeployIndex::new(&dir.path().join("deploy-index.json")));

        let outcome = exec_batch_group(
            &bg,
            "dev",
            &SiblingLeakBatchTarget,
            &BTreeMap::from([
                ("SHORT:dev".to_string(), "short-secret".to_string()),
                ("LONG:dev".to_string(), "short-secret-suffix".to_string()),
            ]),
            b"test-master-key",
            &index,
        );

        for error in outcome.items.iter().filter_map(|(_, error)| error.as_ref()) {
            assert!(!error.contains("short-secret"), "{error}");
            assert!(!error.contains("suffix"), "{error}");
        }
        for record in index.lock().unwrap().records.values() {
            let error = record.last_error.as_deref().unwrap_or_default();
            assert!(!error.contains("short-secret"), "{error}");
            assert!(!error.contains("suffix"), "{error}");
        }
    }

    impl crate::targets::DeployTarget for ParallelTarget {
        fn name(&self) -> &str {
            self.name
        }

        fn deploy_mode(&self) -> crate::targets::DeployMode {
            crate::targets::DeployMode::Individual
        }

        fn deploy_secret(
            &self,
            _key: &str,
            _value: &str,
            _target: &crate::config::ResolvedTarget,
        ) -> anyhow::Result<()> {
            self.started.fetch_add(1, Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(2);
            while self.started.load(Ordering::SeqCst) < 2 {
                if Instant::now() >= deadline {
                    anyhow::bail!("targets were not executed in parallel");
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            if self.succeeds {
                Ok(())
            } else {
                anyhow::bail!("intentional mixed-outcome failure")
            }
        }
    }

    #[test]
    fn animated_parallel_deploy_preserves_mixed_outcomes_by_target() {
        let started = Arc::new(AtomicUsize::new(0));
        let deploy_targets: Vec<Box<dyn crate::targets::DeployTarget>> = vec![
            Box::new(ParallelTarget {
                name: "fail",
                started: Arc::clone(&started),
                succeeds: false,
            }),
            Box::new(ParallelTarget {
                name: "ok",
                started,
                succeeds: true,
            }),
        ];
        let target_map = HashMap::from([
            ("fail", (0, crate::targets::DeployMode::Individual)),
            ("ok", (1, crate::targets::DeployMode::Individual)),
        ]);
        let mut env_plan = EnvWorkPlan::default();
        env_plan.individual.push((
            "SHARED".to_string(),
            zeroize::Zeroizing::new("secret".to_string()),
            crate::config::ResolvedTarget {
                service: "fail".to_string(),
                app: None,
                environment: "dev".to_string(),
            },
        ));
        env_plan.individual.push((
            "SHARED".to_string(),
            zeroize::Zeroizing::new("secret".to_string()),
            crate::config::ResolvedTarget {
                service: "ok".to_string(),
                app: None,
                environment: "dev".to_string(),
            },
        ));
        let key_lines = build_key_lines(&env_plan, &[]);
        let index_dir = tempfile::tempdir().unwrap();
        let index = Mutex::new(DeployIndex::new(
            &index_dir.path().join("deploy-index.json"),
        ));
        let payload_secrets = BTreeMap::from([("SHARED:dev".to_string(), "secret".to_string())]);
        let failed_batch_groups = Mutex::new(BTreeSet::new());
        let mut deployed = Vec::new();
        let mut failed = Vec::new();
        let mut pruned = Vec::new();

        execute_animated(
            "dev",
            &env_plan,
            &key_lines,
            &[],
            DEPLOY_LINE_WIDTH,
            &deploy_targets,
            &target_map,
            &payload_secrets,
            b"test-master-key",
            &index,
            &failed_batch_groups,
            &mut deployed,
            &mut failed,
            &mut pruned,
        );

        assert_eq!(deployed.len(), 1);
        assert_eq!(deployed[0].target, "ok");
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].target, "fail");

        let index = index.into_inner().unwrap();
        assert_eq!(index.records.len(), 2);
        assert!(index.records.values().any(|r| {
            r.target == "ok:dev"
                && r.last_deploy_status == crate::deploy_tracker::DeployStatus::Success
        }));
        assert!(index.records.values().any(|r| {
            r.target == "fail:dev"
                && r.last_deploy_status == crate::deploy_tracker::DeployStatus::Failed
        }));
    }
}
