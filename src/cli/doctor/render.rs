use anyhow::{bail, Result};
use console::style;

use crate::config::Config;
use crate::remotes::render_remote_health;
use crate::targets::{render_target_health, CommandRunner, HealthStatus};
use crate::ui;

use super::types::{CheckStatus, Report, Section};

impl Report {
    pub(crate) fn render(&self, runner: &dyn CommandRunner) -> Result<()> {
        let project_label = self
            .project
            .as_deref()
            .unwrap_or("unknown project");

        cliclack::intro(style(format!("esk doctor · {project_label}")).bold().to_string())?;

        // --- Project structure ---
        let structure_lines = render_checks(&self.structure);
        cliclack::log::step(format!("Project structure\n{}", structure_lines.join("\n")))?;

        // Load config for target/remote health rendering (if available)
        let config = self.project.as_ref().and_then(|_| {
            let config_path = self.root.join("esk.yaml");
            Config::load(&config_path).ok()
        });

        // --- Config ---
        match &self.config {
            Section::Checked(checks) => {
                let lines = render_checks(checks);
                cliclack::log::step(format!("Config\n{}", lines.join("\n")))?;
            }
            Section::Skipped(reason) => {
                cliclack::log::step(format!(
                    "Config\n  {} {}",
                    style("—").dim(),
                    style(format!("skipped: {reason}")).dim()
                ))?;
            }
        }

        // --- Target & Remote health (live animated) ---
        let mut target_ok = 0usize;
        let mut target_fail = 0usize;
        let mut remote_ok = 0usize;
        let mut remote_fail = 0usize;

        if let Some(ref cfg) = config {
            if !cfg.typed_targets.is_empty() {
                let health = render_target_health(cfg, runner, "Targets");
                for h in &health {
                    if h.status.is_ok() {
                        target_ok += 1;
                    } else {
                        target_fail += 1;
                    }
                }
            }

            if !cfg.typed_remotes.is_empty() {
                let health = render_remote_health(cfg, runner, "Remotes");
                for h in &health {
                    match &h.status {
                        HealthStatus::Ok(_) => remote_ok += 1,
                        HealthStatus::Failed(_) => remote_fail += 1,
                    }
                }
            }
        }

        // --- Store consistency ---
        match &self.store_consistency {
            Section::Checked(checks) => {
                let lines = render_checks(checks);
                cliclack::log::step(format!("Store consistency\n{}", lines.join("\n")))?;
            }
            Section::Skipped(reason) => {
                cliclack::log::step(format!(
                    "Store consistency\n  {} {}",
                    style("—").dim(),
                    style(format!("skipped: {reason}")).dim()
                ))?;
            }
        }

        // --- Secrets health ---
        match &self.secrets_health {
            Section::Checked(checks) => {
                let lines = render_checks(checks);
                cliclack::log::step(format!("Secrets\n{}", lines.join("\n")))?;
            }
            Section::Skipped(reason) => {
                cliclack::log::step(format!(
                    "Secrets\n  {} {}",
                    style("—").dim(),
                    style(format!("skipped: {reason}")).dim()
                ))?;
            }
        }

        // --- Suggestions ---
        if !self.suggestions.is_empty() {
            let cmd_width = self
                .suggestions
                .iter()
                .map(|s| s.command.len())
                .max()
                .unwrap_or(0);

            let lines: Vec<String> = self
                .suggestions
                .iter()
                .map(|s| {
                    format!(
                        "  {}  {}",
                        style(format!("{:<width$}", s.command, width = cmd_width)).cyan(),
                        style(&s.reason).dim()
                    )
                })
                .collect();
            cliclack::log::step(format!("Suggestions\n{}", lines.join("\n")))?;
        }

        // --- Summary ---
        let (mut pass, mut warn, mut fail) = count_checks(&self.structure);
        count_section(&self.config, &mut pass, &mut warn, &mut fail);
        count_section(&self.store_consistency, &mut pass, &mut warn, &mut fail);
        count_section(&self.secrets_health, &mut pass, &mut warn, &mut fail);

        // Include target/remote counts
        pass += target_ok + remote_ok;
        fail += target_fail + remote_fail;

        let summary = format!("{pass} passed, {warn} warnings, {fail} failures");

        if fail > 0 {
            cliclack::outro(style(&summary).red().bold().to_string())?;
            bail!("{summary}");
        }

        let outro_style = if warn > 0 {
            style(&summary).yellow()
        } else {
            style(&summary).green()
        };
        cliclack::outro(outro_style.to_string())?;

        Ok(())
    }
}

fn render_checks(checks: &[super::types::Check]) -> Vec<String> {
    checks
        .iter()
        .map(|c| {
            let icon = match c.status {
                CheckStatus::Pass => ui::Icon::Success,
                CheckStatus::Warn => ui::Icon::Warning,
                CheckStatus::Fail => ui::Icon::Failure,
            };
            format!("  {} {}  {}", icon, c.label, style(&c.detail).dim())
        })
        .collect()
}

fn count_checks(checks: &[super::types::Check]) -> (usize, usize, usize) {
    let mut pass = 0;
    let mut warn = 0;
    let mut fail = 0;
    for c in checks {
        match c.status {
            CheckStatus::Pass => pass += 1,
            CheckStatus::Warn => warn += 1,
            CheckStatus::Fail => fail += 1,
        }
    }
    (pass, warn, fail)
}

fn count_section(section: &Section, pass: &mut usize, warn: &mut usize, fail: &mut usize) {
    if let Section::Checked(checks) = section {
        let (p, w, f) = count_checks(checks);
        *pass += p;
        *warn += w;
        *fail += f;
    }
}
