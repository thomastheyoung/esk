use anyhow::Result;
use console::style;
use std::collections::{BTreeMap, BTreeSet};

use crate::ui;

use super::types::DEPLOY_LINE_WIDTH;

#[derive(Default)]
pub(crate) struct EnvStatus {
    pub keys: usize,
    pub deployed: usize,
    pub failed: usize,
    pub unset: usize,
    pub pruned: usize,
}

/// A single deploy result entry for display.
pub(crate) struct DeployEntry {
    pub key: String,
    pub env: String,
    pub target: String,
    pub error: Option<String>,
}

pub(crate) struct DeployReport {
    pub deployed: Vec<DeployEntry>,
    pub failed: Vec<DeployEntry>,
    pub skipped: Vec<DeployEntry>,
    pub unset: Vec<DeployEntry>,
    pub pruned: Vec<DeployEntry>,
    pub unavailable_orphans: Vec<crate::orphan::TargetOrphan>,
    pub dry_run: bool,
    pub verbose: bool,
}

impl DeployReport {
    pub fn is_empty(&self) -> bool {
        self.deployed.is_empty()
            && self.failed.is_empty()
            && self.skipped.is_empty()
            && self.unset.is_empty()
            && self.pruned.is_empty()
    }

    pub fn has_failures(&self) -> bool {
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

    pub fn render(&self) -> Result<()> {
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
                env_status.entry(env.clone()).or_default().keys += 1;
                let label = format!("{} {}", ui::Icon::Success, style(&key).dim());
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
                let label = format!("{} {}", ui::Icon::Failure, style(&key).dim());
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
                let label = format!("{} {}", ui::Icon::Unset, style(&key).dim());
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
                let label = format!("{} {}", ui::Icon::Pruned, style(&key).dim());
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
                let status_summary =
                    ui::format_deploy_summary(es.keys, es.deployed, es.failed, es.unset, es.pruned);

                lines.push(String::new());
                let status_icon = if es.failed > 0 {
                    ui::Icon::Failure.to_string()
                } else {
                    ui::Icon::Pending.color(ui::SectionColor::Green)
                };
                lines.push(format!("{status_icon} {status_summary}"));

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

    pub fn render_skipped(&self) -> Result<()> {
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
pub(crate) fn group_entries(entries: &[DeployEntry]) -> BTreeMap<(String, String), GroupedTargets> {
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
