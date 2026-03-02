mod execute;
mod plan;
pub(crate) mod report;
mod types;

use anyhow::Result;
use console::style;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;

use crate::config::Config;
use crate::deploy_tracker::DeployIndex;
use crate::store::SecretStore;
use crate::targets::{build_targets, CommandRunner, RealCommandRunner};
use crate::ui;

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

pub fn run(config: &Config, opts: &DeployOptions<'_>) -> Result<()> {
    let version = SecretStore::open(&config.root)?.payload()?.version;
    let target_count = config.target_names().len();
    let scope = match opts.env {
        Some(e) => format!(
            "{} target{} · {}",
            target_count,
            if target_count == 1 { "" } else { "s" },
            e
        ),
        None => format!(
            "{} target{}",
            target_count,
            if target_count == 1 { "" } else { "s" }
        ),
    };
    cliclack::intro(
        style(format!(
            "{} · {} · {}",
            style(&config.project).bold(),
            style(format!("v{version}")).dim(),
            scope,
        ))
        .to_string(),
    )?;
    run_with_runner(config, opts, &RealCommandRunner)?;
    let payload = SecretStore::open(&config.root)?.payload()?;
    let env_versions: Vec<(String, u64)> = config
        .environments
        .iter()
        .map(|e| (e.clone(), payload.env_version(e)))
        .collect();
    cliclack::outro(
        style(ui::format_store_outro(
            payload.version,
            &env_versions,
            opts.env,
        ))
        .dim()
        .to_string(),
    )?;
    Ok(())
}

pub fn run_with_runner(
    config: &Config,
    opts: &DeployOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<()> {
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

    // Build a lookup map: target_name -> (index, deploy_mode)
    let target_map: HashMap<&str, (usize, crate::targets::DeployMode)> = deploy_targets
        .iter()
        .enumerate()
        .map(|(i, a)| (a.name(), (i, a.deploy_mode())))
        .collect();

    let plan_output = plan::plan_deploy(
        config,
        &payload,
        &index,
        &resolved,
        &deploy_targets,
        &target_map,
        opts,
    )?;

    if plan_output.is_empty() && plan_output.unset.is_empty() {
        let report = report::DeployReport {
            deployed: Vec::new(),
            failed: Vec::new(),
            skipped: plan_output.skipped,
            unset: plan_output.unset,
            pruned: Vec::new(),
            unavailable_orphans: plan_output.unavailable_orphans,
            dry_run: opts.dry_run,
            verbose: opts.verbose,
        };
        report.render()?;
        return Ok(());
    }

    let index = Mutex::new(index);
    let exec_report = execute::execute_deploy(
        &plan_output,
        &deploy_targets,
        &target_map,
        &payload.secrets,
        &index,
        opts,
    )?;

    let report = execute::build_report(exec_report, plan_output);

    // Determine rendering mode
    let is_tty = std::io::stderr().is_terminal();
    let animated = !opts.verbose && !opts.dry_run && is_tty;
    execute::render_report(&report, animated)?;

    // Warn about orphans whose target is no longer configured
    if !report.unavailable_orphans.is_empty() {
        let lines: Vec<String> = report
            .unavailable_orphans
            .iter()
            .map(|o| format!("  {} → {} ({})", o.key, o.target_display(), o.env))
            .collect();
        cliclack::log::warning(format!(
            "Cannot prune — target no longer configured:\n{}\n  \
             Remove these manually or re-add the target config.",
            lines.join("\n")
        ))?;
    }

    if !opts.dry_run {
        index.lock().unwrap().save()?;
    }

    if report.has_failures() {
        anyhow::bail!("{} deploy(s) failed", report.failed.len());
    }

    Ok(())
}
