use anyhow::Result;
use console::style;
use std::collections::BTreeSet;

use crate::config::Config;
use crate::targets::{render_target_health, CommandRunner};
use crate::ui;

use super::types::*;

impl Dashboard {
    pub(crate) fn render(
        &self,
        config: &Config,
        runner: &dyn CommandRunner,
        all: bool,
    ) -> Result<()> {
        // When filtering a single env, show that env's version; otherwise global
        let display_version = match &self.filtered_env {
            Some(env) => self
                .env_versions
                .iter()
                .find(|(e, _)| e == env)
                .map_or(self.version, |(_, v)| *v),
            None => self.version,
        };

        // Summary line
        let total = self.failed.len() + self.pending.len() + self.deployed.len() + self.unset.len();
        let summary = if total == 0 {
            format!(
                "{} · {}",
                style(&self.project).bold(),
                style(format!("v{display_version}")).dim(),
            )
        } else {
            let parts = ui::format_count_summary(&[
                ("deployed", self.deployed.len()),
                ("pending", self.pending.len()),
                ("failed", self.failed.len()),
                ("unset", self.unset.len()),
                ("invalid", self.validation_warnings.len()),
                ("cross-field", self.cross_field_violations.len()),
                ("empty", self.empty_values.len()),
                ("required missing", self.missing_required.len()),
                ("target orphans", self.target_orphans.len()),
            ]);

            let all_deployed = self.failed.is_empty()
                && self.pending.is_empty()
                && self.unset.is_empty()
                && self.validation_warnings.is_empty()
                && self.cross_field_violations.is_empty()
                && self.empty_values.is_empty()
                && self.missing_required.is_empty()
                && self.target_orphans.is_empty();

            if all_deployed {
                format!(
                    "{} · {} · {}, all deployed",
                    style(&self.project).bold(),
                    style(format!("v{display_version}")).dim(),
                    style(format!(
                        "{total} target{}",
                        if total == 1 { "" } else { "s" }
                    )),
                )
            } else {
                format!(
                    "{} · {} · {} target{} ({})",
                    style(&self.project).bold(),
                    style(format!("v{display_version}")).dim(),
                    total,
                    if total == 1 { "" } else { "s" },
                    parts,
                )
            }
        };

        cliclack::intro(style(summary).to_string())?;

        // Target health checks (parallel, animated)
        render_target_health(config, runner, "Targets");

        // Deploy section
        let has_problems =
            !self.failed.is_empty() || !self.pending.is_empty() || !self.unset.is_empty();

        if has_problems || (all && !self.deployed.is_empty()) {
            let mut deploy_lines = Vec::new();

            // Compute max key:env width across all deploy sub-sections for alignment
            let empty = Vec::new();
            let deployed_ref = if all { &self.deployed } else { &empty };
            let deploy_label_width = self
                .failed
                .iter()
                .chain(self.pending.iter())
                .chain(self.unset.iter())
                .chain(deployed_ref.iter())
                .map(|e| e.key.len() + 1 + e.env.len()) // "key:env"
                .max()
                .unwrap_or(0);

            if !self.failed.is_empty() {
                deploy_lines.push(ui::section_header(
                    ui::Icon::Failure,
                    &format!("{} failed", self.failed.len()),
                    ui::SectionColor::Red,
                ));
                for entry in &self.failed {
                    let freshness = entry
                        .last_deployed_at
                        .as_deref()
                        .map(ui::format_relative_time)
                        .unwrap_or_default();
                    let err_text = entry
                        .error
                        .as_deref()
                        .map(|e| format!(" {}", style(format!("({e})")).dim()))
                        .unwrap_or_default();
                    deploy_lines.push(ui::section_entry(
                        &format!("{}:{}", entry.key, entry.env),
                        &format!("→ {}  {}{}", entry.target, style(freshness).dim(), err_text,),
                        deploy_label_width,
                    ));
                }
            }

            if !self.pending.is_empty() {
                deploy_lines.push(ui::section_header(
                    ui::Icon::Pending,
                    &format!("{} pending", self.pending.len()),
                    ui::SectionColor::Yellow,
                ));
                let groups = group_entries(&self.pending, TimestampPick::Oldest);
                let shown = if all {
                    groups.len()
                } else {
                    groups.len().min(ui::TRUNCATE_LIMIT)
                };
                for group in groups.iter().take(shown) {
                    let targets = group.targets.join(", ");
                    let freshness = match &group.freshness {
                        GroupedFreshness::NeverDeployed => "never deployed".to_string(),
                        GroupedFreshness::Timestamp(ts) => {
                            let ago = ui::format_relative_time(ts);
                            format!("last deployed {ago}")
                        }
                    };
                    deploy_lines.push(ui::section_entry(
                        &format!("{}:{}", group.key, group.env),
                        &format!("→ {}  {}", targets, style(freshness).dim()),
                        deploy_label_width,
                    ));
                }
                if let Some(footer) = ui::truncation_footer(groups.len(), shown) {
                    deploy_lines.push(footer);
                }
            }

            if !self.unset.is_empty() {
                deploy_lines.push(ui::section_header(
                    ui::Icon::Unset,
                    &format!("{} unset", self.unset.len()),
                    ui::SectionColor::Dim,
                ));
                let groups = group_entries(&self.unset, TimestampPick::Oldest);
                let shown = if all {
                    groups.len()
                } else {
                    groups.len().min(ui::TRUNCATE_LIMIT)
                };
                for group in groups.iter().take(shown) {
                    let targets = group.targets.join(", ");
                    deploy_lines.push(ui::section_entry(
                        &format!("{}:{}", group.key, group.env),
                        &format!("→ {}", style(targets).dim()),
                        deploy_label_width,
                    ));
                }
                if let Some(footer) = ui::truncation_footer(groups.len(), shown) {
                    deploy_lines.push(footer);
                }
            }

            if !self.deployed.is_empty() {
                if all {
                    deploy_lines.push(ui::section_header(
                        ui::Icon::Success,
                        &format!("{} deployed", self.deployed.len()),
                        ui::SectionColor::Green,
                    ));
                    let groups = group_entries(&self.deployed, TimestampPick::Newest);
                    for group in &groups {
                        let targets = group.targets.join(", ");
                        let freshness = match &group.freshness {
                            GroupedFreshness::NeverDeployed => String::new(),
                            GroupedFreshness::Timestamp(ts) => ui::format_relative_time(ts),
                        };
                        deploy_lines.push(ui::section_entry(
                            &format!("{}:{}", group.key, group.env),
                            &format!("→ {}  {}", targets, style(freshness).dim()),
                            deploy_label_width,
                        ));
                    }
                } else {
                    deploy_lines.push(format!(
                        "  {} {}  {}",
                        ui::Icon::Success,
                        style(format!("{} deployed", self.deployed.len())).green(),
                        style("(--all to show)").dim()
                    ));
                }
            }

            if !deploy_lines.is_empty() {
                cliclack::log::step(format!("Deploy (targets)\n{}", deploy_lines.join("\n")))?;
            }
        }

        // Validation section
        let has_validation =
            !self.validation_warnings.is_empty() || !self.cross_field_violations.is_empty();
        if has_validation {
            let mut val_lines = Vec::new();
            if !self.validation_warnings.is_empty() {
                val_lines.push(ui::section_header(
                    ui::Icon::Warning,
                    &format!("{} invalid", self.validation_warnings.len()),
                    ui::SectionColor::Yellow,
                ));
                for w in &self.validation_warnings {
                    val_lines.push(ui::section_entry(
                        &format!("{}:{}", w.key, w.env),
                        &style(&w.message).dim().to_string(),
                        0,
                    ));
                }
            }
            if !self.cross_field_violations.is_empty() {
                val_lines.push(ui::section_header(
                    ui::Icon::Warning,
                    &format!("{} cross-field", self.cross_field_violations.len()),
                    ui::SectionColor::Yellow,
                ));
                for v in &self.cross_field_violations {
                    val_lines.push(ui::section_entry(
                        &format!("{}:{}", v.key, v.env),
                        &style(&v.message).dim().to_string(),
                        0,
                    ));
                }
            }
            cliclack::log::step(format!("Validation\n{}", val_lines.join("\n")))?;
        }

        // Empty values section
        if !self.empty_values.is_empty() {
            let mut empty_lines = Vec::new();
            empty_lines.push(ui::section_header(
                ui::Icon::Warning,
                &format!("{} empty", self.empty_values.len()),
                ui::SectionColor::Yellow,
            ));
            for w in &self.empty_values {
                empty_lines.push(ui::section_entry(
                    &format!("{}:{}", w.key, w.env),
                    &style(w.kind).dim().to_string(),
                    0,
                ));
            }
            cliclack::log::step(format!("Empty values\n{}", empty_lines.join("\n")))?;
        }

        // Required section
        if !self.missing_required.is_empty() {
            let mut req_lines = Vec::new();
            req_lines.push(ui::section_header(
                ui::Icon::Warning.color(ui::SectionColor::Red),
                &format!("{} required missing", self.missing_required.len()),
                ui::SectionColor::Red,
            ));
            let shown = if all {
                self.missing_required.len()
            } else {
                self.missing_required.len().min(ui::TRUNCATE_LIMIT)
            };
            for m in self.missing_required.iter().take(shown) {
                let target_info = if m.targets.is_empty() {
                    String::new()
                } else {
                    format!("  {}", style(format!("({})", m.targets.join(", "))).dim())
                };
                req_lines.push(ui::section_entry(
                    &format!("{}:{}", m.key, m.env),
                    &target_info,
                    0,
                ));
            }
            if let Some(footer) = ui::truncation_footer(self.missing_required.len(), shown) {
                req_lines.push(footer);
            }
            cliclack::log::step(format!("Requirements\n{}", req_lines.join("\n")))?;
        }

        // Coverage section — deduplicate gaps already shown in Requirements
        let required_set: BTreeSet<(&str, &str)> = self
            .missing_required
            .iter()
            .map(|m| (m.key.as_str(), m.env.as_str()))
            .collect();

        // Build filtered coverage gap lines
        let mut coverage_gap_lines: Vec<String> = Vec::new();
        let mut coverage_gap_count = 0usize;
        for gap in &self.coverage_gaps {
            let filtered_envs: Vec<&String> = gap
                .missing_envs
                .iter()
                .filter(|e| !required_set.contains(&(gap.key.as_str(), e.as_str())))
                .collect();
            if filtered_envs.is_empty() {
                continue;
            }
            coverage_gap_count += 1;
            let present = gap.present_envs.join(", ");
            for missing_env in &filtered_envs {
                coverage_gap_lines.push(ui::section_entry(
                    &gap.key,
                    &format!(
                        "missing in {} {}",
                        style(missing_env).yellow(),
                        style(format!("(set in {present})")).dim(),
                    ),
                    0,
                ));
            }
        }

        let has_coverage = !coverage_gap_lines.is_empty()
            || !self.orphans.is_empty()
            || !self.target_orphans.is_empty();
        if has_coverage {
            let mut cov_lines = Vec::new();

            if !coverage_gap_lines.is_empty() {
                cov_lines.push(ui::section_header(
                    ui::Icon::Unset,
                    &format!("{coverage_gap_count} declared but never set"),
                    ui::SectionColor::Dim,
                ));
                let shown = if all {
                    coverage_gap_lines.len()
                } else {
                    coverage_gap_lines.len().min(ui::TRUNCATE_LIMIT)
                };
                cov_lines.extend(coverage_gap_lines.iter().take(shown).cloned());
                if let Some(footer) = ui::truncation_footer(coverage_gap_lines.len(), shown) {
                    cov_lines.push(footer);
                }
            }

            if !self.orphans.is_empty() {
                cov_lines.push(ui::section_header(
                    ui::Icon::Warning,
                    &format!("{} in store, not in config", self.orphans.len()),
                    ui::SectionColor::Yellow,
                ));
                for orphan in &self.orphans {
                    cov_lines.push(ui::section_entry(
                        &format!("{}:{}", orphan.key, orphan.env),
                        "",
                        0,
                    ));
                }
            }

            if !self.target_orphans.is_empty() {
                cov_lines.push(ui::section_header(
                    ui::Icon::Warning,
                    &format!(
                        "{} deployed but no longer in config",
                        self.target_orphans.len()
                    ),
                    ui::SectionColor::Yellow,
                ));
                for orphan in &self.target_orphans {
                    let target_display = orphan.target_display();
                    let freshness = ui::format_relative_time(&orphan.last_deployed_at);
                    cov_lines.push(ui::section_entry(
                        &orphan.key,
                        &format!(
                            "{} {}  {}",
                            style("→").dim(),
                            style(format!("{} ({})", target_display, orphan.env)),
                            style(freshness).dim(),
                        ),
                        0,
                    ));
                }
            }

            cliclack::log::step(format!("Coverage\n{}", cov_lines.join("\n")))?;
        }

        // Sync section (remotes)
        if !self.remote_states.is_empty() {
            let sync_label_width = self
                .remote_states
                .iter()
                .map(|ps| ps.name.len() + 1 + ps.env.len())
                .max()
                .unwrap_or(0);

            let lines: Vec<String> = self
                .remote_states
                .iter()
                .map(|ps| {
                    let label = format!("{}:{}", ps.name, ps.env);
                    let pad = sync_label_width.saturating_sub(label.len());
                    match &ps.status {
                        RemoteStatus::Current { version } => format!(
                            "  {} {}{}  {}",
                            ui::Icon::Success,
                            style(&label),
                            " ".repeat(pad),
                            style(format!("v{version}")).dim()
                        ),
                        RemoteStatus::Stale { pushed, local } => format!(
                            "  {} {}{}  {}",
                            ui::Icon::Pending,
                            style(&label),
                            " ".repeat(pad),
                            style(format!("v{pushed} → local v{local}")).dim()
                        ),
                        RemoteStatus::Failed { version, error } => format!(
                            "  {} {}{}  {} {}",
                            ui::Icon::Failure,
                            style(&label),
                            " ".repeat(pad),
                            style(format!("v{version}")).dim(),
                            style(format!("({error})")).dim()
                        ),
                        RemoteStatus::NeverSynced => format!(
                            "  {} {}{}  {}",
                            ui::Icon::Unset,
                            style(&label),
                            " ".repeat(pad),
                            style("never synced").dim()
                        ),
                    }
                })
                .collect();
            cliclack::log::step(format!("Sync (remotes)\n{}", lines.join("\n")))?;
        }

        // Next steps section
        if !self.next_steps.is_empty() {
            let cmd_width = self
                .next_steps
                .iter()
                .map(|s| s.command.len())
                .max()
                .unwrap_or(0);

            let lines: Vec<String> = self
                .next_steps
                .iter()
                .map(|s| {
                    format!(
                        "  {}  {}",
                        style(format!("{:<width$}", s.command, width = cmd_width)).cyan(),
                        style(&s.description).dim()
                    )
                })
                .collect();
            cliclack::log::step(format!("Next steps\n{}", lines.join("\n")))?;
        }

        let outro_text = ui::format_store_outro(
            self.version,
            &self.env_versions,
            self.filtered_env.as_deref(),
        );
        cliclack::outro(style(outro_text).dim().to_string())?;

        Ok(())
    }
}
