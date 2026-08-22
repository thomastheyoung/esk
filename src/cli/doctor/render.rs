use anyhow::{bail, Result};
use console::style;

use crate::config::Config;
use crate::remotes::render_remote_health;
use crate::targets::{render_target_health, CommandRunner, HealthStatus};
use crate::ui;

use super::types::{Check, CheckStatus, Report, Section};

impl Report {
    pub(crate) fn render(&self, runner: &dyn CommandRunner) -> Result<()> {
        let project_label = self.project.as_deref().unwrap_or("unknown project");

        cliclack::intro(
            style(format!("esk doctor · {project_label}"))
                .bold()
                .to_string(),
        )?;

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
        let mut vault_checks: Vec<Check> = Vec::new();

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

                vault_checks = vault_isolation_checks(cfg, runner);
                if !vault_checks.is_empty() {
                    render_checked_section(&term, &bar, "1Password", &vault_checks)?;
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
        let mut tally = self.tally();
        tally.add_health(target_ok + remote_ok, target_fail + remote_fail);
        // Vault isolation is computed live, so it is not part of `Report` and
        // must be folded in here — otherwise the warning prints but the summary
        // line and exit code ignore it.
        tally.add_checks(&vault_checks);

        let summary = tally.summary();

        if tally.has_failures() {
            cliclack::outro(style(&summary).red().bold().to_string())?;
            bail!("{summary}");
        }

        let outro_style = if tally.warn > 0 {
            style(&summary).yellow()
        } else {
            style(&summary).green()
        };
        cliclack::outro(outro_style.to_string())?;

        Ok(())
    }
}

/// Checks on the configured 1Password vault. Empty unless 1Password is set up.
///
/// esk's in-process guard stops esk from *asking* for a foreign item, but the
/// `op` session it inherits can still reach one. A vault containing only esk
/// items — ideally reached by a service account scoped to it — is the only
/// enforcement that survives a bug in esk, so doctor names the gap.
fn vault_isolation_checks(config: &Config, runner: &dyn CommandRunner) -> Vec<Check> {
    let op_config = match config.try_onepassword_remote_config() {
        None => return Vec::new(),
        Some(Ok(cfg)) => cfg,
        Some(Err(e)) => {
            return vec![Check::fail(
                "Vault isolation",
                format!("1password remote config is malformed: {e}"),
            )]
        }
    };
    let remote = crate::remotes::onepassword::OnePasswordRemote::new(config, op_config, runner);
    let composition = match remote.vault_composition() {
        Ok(c) => c,
        Err(e) => {
            // Don't fall silent: an unreadable vault reads as "all clear" to
            // anyone scanning the output, which is the opposite of the truth.
            return vec![Check::warn(
                "Vault isolation",
                format!("could not determine: {}", root_cause(&e)),
            )];
        }
    };

    let mut checks = vec![if composition.is_isolated() {
        Check::pass(
            "Vault isolation",
            format!(
                "vault '{}' holds only esk items ({})",
                composition.vault, composition.esk_owned
            ),
        )
    } else {
        Check::warn(
            "Vault isolation",
            format!(
                "vault '{}' holds {} item(s) esk does not own — a dedicated vault limits what esk's 1Password session can reach",
                composition.vault, composition.foreign
            ),
        )
    }];

    // A duplicated ownership tag leaves no safe item ID to select. Operational
    // reads and writes fail closed on the same condition.
    if composition.duplicate_owned > 0 {
        checks.push(Check::fail(
            "Item ambiguity",
            format!(
                "{} esk ownership tag(s) match more than one item in vault '{}' — delete the duplicate or remove its esk tag",
                composition.duplicate_owned, composition.vault
            ),
        ));
    }

    checks
}

/// Innermost cause of an error chain, for a one-line check detail.
fn root_cause(e: &anyhow::Error) -> String {
    e.chain()
        .last()
        .map_or_else(|| e.to_string(), std::string::ToString::to_string)
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
