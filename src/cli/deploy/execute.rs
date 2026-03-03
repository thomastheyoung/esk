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
        dry_run,
        verbose,
    })
}

/// Build the final report by merging execution results with plan outputs.
pub(crate) fn build_report(mut exec_report: DeployReport, plan: PlanOutput) -> DeployReport {
    exec_report.skipped = plan.skipped;
    exec_report.unset = plan.unset;
    exec_report.unavailable_orphans = plan.unavailable_orphans;
    exec_report
}

/// Render post-execution output depending on animated vs sequential mode.
pub(crate) fn render_report(report: &DeployReport, animated: bool) -> Result<()> {
    if animated {
        if !report.skipped.is_empty() {
            if report.verbose {
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
    Ok(())
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
            let target = crate::config::ResolvedTarget {
                service: bg.target_name.clone(),
                app: bg.app.clone(),
                environment: env_name.to_string(),
            };
            let target_display = target.target_display();

            s.spawn(move || {
                let batch_results = deploy_target.deploy_batch(&bg.secrets, &target);

                let mut idx = index.lock().expect("deploy index mutex poisoned");
                let mut res = results.lock().expect("results mutex poisoned");

                // Track if any result in this batch failed
                let mut batch_had_failure = false;

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
                            idx.record_success(tracker_key, target.to_string(), value_hash);
                            if let Some(kr) = res.get_mut(&result.key) {
                                kr.completed_ops += 1;
                            }
                        } else {
                            batch_had_failure = true;
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
                                kr.failed.push((target_display.clone(), error));
                            }
                        }
                    }
                }

                // BUG FIX (esk-0vf): Record batch failures so prune workers
                // can skip pruning for failed batch groups.
                if batch_had_failure {
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
                let result = deploy_target.deploy_secret(key, value, target);
                let tracker_key = DeployIndex::tracker_key(
                    key,
                    &target.service,
                    target.app.as_deref(),
                    &target.environment,
                );
                let value_hash = DeployIndex::hash_value(value);

                let mut idx = index.lock().expect("deploy index mutex poisoned");
                let mut res = results.lock().expect("results mutex poisoned");
                let target_display = target.target_display();

                match result {
                    Ok(()) => {
                        idx.record_success(tracker_key, target.to_string(), value_hash);
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
            });
        }

        // Tombstone delete workers
        for (key, target) in &plan.tombstones {
            let results = &results;
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

                let mut idx = index.lock().expect("deploy index mutex poisoned");
                let mut res = results.lock().expect("results mutex poisoned");
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
            });
        }

        // Batch prune workers
        for ((target_name, app), orphan_list) in &plan.batch_prune {
            let results = &results;
            let group_key = (target_name.clone(), app.clone(), env_name.to_string());

            s.spawn(move || {
                let mut idx = index.lock().expect("deploy index mutex poisoned");
                let mut res = results.lock().expect("results mutex poisoned");

                if failed_batch_groups
                    .lock()
                    .expect("failed batch groups mutex poisoned")
                    .contains(&group_key)
                {
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
                                kr.failed.push((orphan.target_display(), e.to_string()));
                            }
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
            let target = crate::config::ResolvedTarget {
                service: orphan.service.clone(),
                app: orphan.app.clone(),
                environment: orphan.env.clone(),
            };

            s.spawn(move || {
                let result = deploy_target.delete_secret(&orphan.key, &target);
                let mut idx = index.lock().expect("deploy index mutex poisoned");
                let mut res = results.lock().expect("results mutex poisoned");
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
                // Count non-failed ops as deployed
                let ok_count = kr.completed_ops.saturating_sub(kr.failed.len());
                for target in kl.targets.iter().take(ok_count) {
                    deployed.push(DeployEntry {
                        key: kl.key.clone(),
                        env: env_name.to_string(),
                        target: target.clone(),
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
    let summary = ui::format_deploy_summary(
        env_keys,
        env_deployed,
        env_failed,
        env_unset_count,
        env_pruned,
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
        let deploy_target = &deploy_targets[bg.target_idx];
        let target = crate::config::ResolvedTarget {
            service: bg.target_name.clone(),
            app: bg.app.clone(),
            environment: env_name.to_string(),
        };
        let target_display = target.target_display();

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
                continue;
            }
            for s in &bg.secrets {
                deployed.push(DeployEntry {
                    key: s.key.clone(),
                    env: env_name.to_string(),
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
        let mut idx = index.lock().expect("deploy index mutex poisoned");

        if batch_results.is_empty() {
            for key in &bg.tombstoned_keys {
                let tracker_key =
                    DeployIndex::tracker_key(key, &bg.target_name, bg.app.as_deref(), env_name);
                idx.record_success(
                    tracker_key,
                    target.to_string(),
                    DeployIndex::TOMBSTONE_HASH.to_string(),
                );
                deployed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
                    target: target_display.clone(),
                    error: None,
                });
            }
            idx.save()?;
            continue;
        }

        for result in &batch_results {
            let tracker_key =
                DeployIndex::tracker_key(&result.key, &bg.target_name, bg.app.as_deref(), env_name);
            let composite = format!("{}:{}", result.key, env_name);
            let value = payload_secrets
                .get(&composite)
                .map_or("", std::string::String::as_str);
            let value_hash = DeployIndex::hash_value(value);

            if result.outcome.is_success() {
                idx.record_success(tracker_key, target.to_string(), value_hash);
                deployed.push(DeployEntry {
                    key: result.key.clone(),
                    env: env_name.to_string(),
                    target: target_display.clone(),
                    error: None,
                });
            } else {
                let error = result
                    .outcome
                    .error_message()
                    .unwrap_or_default()
                    .to_string();
                idx.record_failure(tracker_key, target.to_string(), value_hash, error.clone());
                failed.push(DeployEntry {
                    key: result.key.clone(),
                    env: env_name.to_string(),
                    target: target_display.clone(),
                    error: Some(error),
                });
                failed_batch_groups
                    .lock()
                    .expect("failed batch groups mutex poisoned")
                    .insert((bg.target_name.clone(), bg.app.clone(), env_name.to_string()));
            }
        }
        idx.save()?;
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
        let mut idx = index.lock().expect("deploy index mutex poisoned");
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
        let result = deploy_target.deploy_secret(key, value, target);

        let tracker_key = DeployIndex::tracker_key(
            key,
            &target.service,
            target.app.as_deref(),
            &target.environment,
        );
        let value_hash = DeployIndex::hash_value(value);

        let mut idx = index.lock().expect("deploy index mutex poisoned");
        match result {
            Ok(()) => {
                idx.record_success(tracker_key, target.to_string(), value_hash);
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
                idx.record_failure(tracker_key, target.to_string(), value_hash, e.to_string());
                failed.push(DeployEntry {
                    key: key.clone(),
                    env: env_name.to_string(),
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
                env: env_name.to_string(),
                target: target_display,
                error: None,
            });
            continue;
        }

        let (target_idx, _) = target_map[target.service.as_str()];
        let deploy_target = &deploy_targets[target_idx];

        let mut idx = index.lock().expect("deploy index mutex poisoned");
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
                    env: env_name.to_string(),
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
                    env: env_name.to_string(),
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
                env: env_name.to_string(),
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

        let mut idx = index.lock().expect("deploy index mutex poisoned");
        match deploy_target.delete_secret(&orphan.key, &target) {
            Ok(()) => {
                idx.remove_record(&orphan.tracker_key);
                pruned.push(DeployEntry {
                    key: orphan.key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: None,
                });
                idx.save()?;
            }
            Err(e) => {
                failed.push(DeployEntry {
                    key: orphan.key.clone(),
                    env: env_name.to_string(),
                    target: target_display,
                    error: Some(e.to_string()),
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
