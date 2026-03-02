use anyhow::{bail, Result};
use console::style;

use crate::config::Config;
use crate::remotes::render_remote_health;
use crate::targets::{render_target_health, CommandRunner, HealthStatus};
use crate::ui;

use super::types::{Check, CheckStatus, Report, Section};

impl Report {
    pub(crate) fn render(&self, runner: &dyn CommandRunner) -> Result<()> {
        let project_label = self
            .project
            .as_deref()
            .unwrap_or("unknown project");

        cliclack::intro(style(format!("esk doctor · {project_label}")).bold().to_string())?;

        let term = console::Term::stderr();
        let bar = style("\u{2502}").dim();

        // --- Project structure ---
        render_checked_section(&term, &bar, "Project structure", &self.structure)?;

        // Load config for target/remote health rendering (if available)
        let config = self.project.as_ref().and_then(|_| {
            let config_path = self.root.join("esk.yaml");
            Config::load(&config_path).ok()
        });

        // --- Config ---
        render_section(&term, &bar, "Config", &self.config)?;

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
        render_section(&term, &bar, "Store consistency", &self.store_consistency)?;

        // --- Secrets health ---
        render_section(&term, &bar, "Secrets", &self.secrets_health)?;

        // --- Suggestions ---
        if !self.suggestions.is_empty() {
            let cmd_width = self
                .suggestions
                .iter()
                .map(|s| s.command.len())
                .max()
                .unwrap_or(0);

            term.write_line(&format!("{}  Suggestions", style("\u{25C7}").dim()))?;
            for s in &self.suggestions {
                term.write_line(&format!(
                    "{bar}    {}  {}",
                    style(format!("{:<width$}", s.command, width = cmd_width)).cyan(),
                    style(&s.reason).dim()
                ))?;
            }
            term.write_line(&format!("{bar}"))?;
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

/// Renders a checked section with a colored filled diamond header and aligned check items.
fn render_checked_section(
    term: &console::Term,
    bar: &console::StyledObject<&str>,
    title: &str,
    checks: &[Check],
) -> std::io::Result<()> {
    let header_icon = section_icon(checks);
    let label_width = checks.iter().map(|c| c.label.len()).max().unwrap_or(0) + 2;

    term.write_line(&format!("{header_icon}  {title}"))?;
    for c in checks {
        let icon = check_icon(c.status);
        term.write_line(&format!(
            "{bar}    {} {:<label_width$}{}",
            icon,
            c.label,
            style(&c.detail).dim(),
        ))?;
    }
    term.write_line(&format!("{bar}"))?;

    Ok(())
}

/// Renders a section that may be checked or skipped.
fn render_section(
    term: &console::Term,
    bar: &console::StyledObject<&str>,
    title: &str,
    section: &Section,
) -> std::io::Result<()> {
    match section {
        Section::Checked(checks) => render_checked_section(term, bar, title, checks),
        Section::Skipped(reason) => {
            term.write_line(&format!("{}  {title}", style("\u{25C7}").dim()))?;
            term.write_line(&format!(
                "{bar}    {} {}",
                style("\u{2014}").dim(),
                style(format!("skipped: {reason}")).dim()
            ))?;
            term.write_line(&format!("{bar}"))?;
            Ok(())
        }
    }
}

/// Returns a colored filled diamond based on the worst status in the checks.
fn section_icon(checks: &[Check]) -> console::StyledObject<&'static str> {
    let all_pass = checks.is_empty() || checks.iter().all(|c| c.status == CheckStatus::Pass);
    let all_fail = !checks.is_empty() && checks.iter().all(|c| c.status == CheckStatus::Fail);

    if all_pass {
        style("\u{25C6}").green()
    } else if all_fail {
        style("\u{25C6}").red()
    } else {
        style("\u{25C6}").yellow()
    }
}

fn check_icon(status: CheckStatus) -> ui::Icon {
    match status {
        CheckStatus::Pass => ui::Icon::Success,
        CheckStatus::Warn => ui::Icon::Warning,
        CheckStatus::Fail => ui::Icon::Failure,
    }
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
