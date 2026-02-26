use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;
use std::io::IsTerminal;

use crate::config::Config;
use crate::reconcile::{self, ConflictPreference};
use crate::remotes::{self, SyncRemote};
use crate::store::{SecretStore, StorePayload};
use crate::suggest;
use crate::sync_tracker::SyncIndex;
use crate::targets::{CommandRunner, RealCommandRunner};
use crate::ui;

#[derive(Clone, Copy)]
pub struct SyncOptions<'a> {
    pub env: Option<&'a str>,
    pub only: Option<&'a str>,
    pub dry_run: bool,
    pub bail: bool,
    pub force: bool,
    pub auto_deploy: bool,
    pub prefer: ConflictPreference,
}

const SYNC_LINE_WIDTH: usize = 30;

pub struct RemotePushResult {
    pub remote: String,
    pub success: bool,
    pub error: Option<String>,
}

fn env_version_label(payload: &StorePayload, env: &str) -> String {
    ui::format_version_label(payload.env_version(env), payload.env_last_changed_at(env))
}

struct PullResult {
    remote: String,
    outcome: PullOutcome,
}

enum PullOutcome {
    Fetched { version: u64, secret_count: usize },
    Empty,
    Failed,
}

enum ReconcileOutcome {
    Merged { version_label: String },
    UpToDate { version_label: String },
    DriftRepair { version_label: String },
}

struct SyncReport {
    env: String,
    pulls: Vec<PullResult>,
    reconcile: ReconcileOutcome,
    pushes: Vec<RemotePushResult>,
    dry_run: bool,
}

impl SyncReport {
    fn has_push_failures(&self) -> bool {
        self.pushes.iter().any(|r| !r.success)
    }

    fn push_failure_count(&self) -> usize {
        self.pushes.iter().filter(|r| !r.success).count()
    }

    fn render(&self) -> Result<()> {
        let mut lines = Vec::new();

        // Pull lines
        for pull in &self.pulls {
            let line = match &pull.outcome {
                PullOutcome::Fetched {
                    version,
                    secret_count,
                } => ui::format_dashboard_line(
                    &format!("↓ {}", pull.remote),
                    &format!("v{version}, {secret_count} secrets"),
                    SYNC_LINE_WIDTH,
                ),
                PullOutcome::Empty => ui::format_dashboard_line(
                    &format!("↓ {}", pull.remote),
                    &style("no data").dim().to_string(),
                    SYNC_LINE_WIDTH,
                ),
                PullOutcome::Failed => ui::format_dashboard_line(
                    &format!("↓ {}", pull.remote),
                    &style("failed").red().to_string(),
                    SYNC_LINE_WIDTH,
                ),
            };
            lines.push(line);
        }

        // Push lines
        if self.dry_run {
            for push in &self.pushes {
                lines.push(ui::format_dashboard_line(
                    &format!("↑ {}", push.remote),
                    &style("would push").dim().to_string(),
                    SYNC_LINE_WIDTH,
                ));
            }
        } else {
            for push in &self.pushes {
                if push.success {
                    lines.push(ui::format_dashboard_line(
                        &format!("↑ {}", push.remote),
                        &style("synced").green().to_string(),
                        SYNC_LINE_WIDTH,
                    ));
                } else {
                    lines.push(ui::format_dashboard_line(
                        &format!("↑ {}", push.remote),
                        &style("failed").red().to_string(),
                        SYNC_LINE_WIDTH,
                    ));
                }
            }
        }

        // Status line
        let status_line = if self.dry_run {
            match &self.reconcile {
                ReconcileOutcome::Merged { version_label } => {
                    format!("{} Would merge → {}", ui::icon_merge(), version_label)
                }
                ReconcileOutcome::UpToDate { version_label } => {
                    format!("{} Up to date → {}", ui::icon_success(), version_label)
                }
                ReconcileOutcome::DriftRepair { version_label } => {
                    format!(
                        "{} Current ({}), would repair drift",
                        ui::icon_merge(),
                        version_label
                    )
                }
            }
        } else {
            match &self.reconcile {
                ReconcileOutcome::Merged { version_label } => {
                    format!("{} Merged → {}", ui::icon_merge(), version_label)
                }
                ReconcileOutcome::UpToDate { version_label } => {
                    format!("{} Up to date → {}", ui::icon_success(), version_label)
                }
                ReconcileOutcome::DriftRepair { .. } => {
                    format!("{} Stale remotes (repairing...)", ui::icon_merge())
                }
            }
        };

        lines.push(String::new());
        lines.push(status_line);
        cliclack::note(&self.env, lines.join("\n"))?;

        Ok(())
    }
}

/// Push a payload to the given remotes, recording results in the remote index.
/// Shows a spinner per remote during push.
pub fn push_to_remotes(
    remotes: &[Box<dyn SyncRemote + '_>],
    payload: &StorePayload,
    config: &Config,
    env: &str,
    sync_index: &mut SyncIndex,
) -> Result<Vec<RemotePushResult>> {
    let mut results = Vec::new();
    let pushed_version = payload.env_version(env);

    for rem in remotes {
        let spinner = cliclack::spinner();
        spinner.start(format!("↑ {}...", rem.name()));

        match rem.push(payload, config, env) {
            Ok(()) => {
                spinner.stop(format!("↑ {}  {}", rem.name(), style("done").green()));
                sync_index.record_success(rem.name(), env, pushed_version);
                results.push(RemotePushResult {
                    remote: rem.name().to_string(),
                    success: true,
                    error: None,
                });
            }
            Err(e) => {
                spinner.error(format!("↑ {}  {} — {e}", rem.name(), style("failed").red()));
                sync_index.record_failure(rem.name(), env, pushed_version, e.to_string());
                results.push(RemotePushResult {
                    remote: rem.name().to_string(),
                    success: false,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    Ok(results)
}

pub fn run(config: &Config, options: SyncOptions<'_>) -> Result<()> {
    let runner = RealCommandRunner;
    let envs: Vec<&str> = match options.env {
        Some(e) => {
            config.validate_env(e)?;
            vec![e]
        }
        None => config
            .environments
            .iter()
            .map(std::string::String::as_str)
            .collect(),
    };

    if envs.len() == 1 {
        return run_with_runner(config, &options, &runner);
    }

    let mut failures: Vec<String> = Vec::new();
    for env in &envs {
        let per_env_opts = SyncOptions {
            env: Some(env),
            ..options
        };
        if let Err(e) = run_with_runner(config, &per_env_opts, &runner) {
            if options.bail {
                bail!("sync failed for environment '{env}': {e}");
            }
            cliclack::log::error(format!("sync failed for environment '{env}': {e}"))?;
            failures.push((*env).to_string());
        }
    }

    if !failures.is_empty() {
        bail!(
            "{} environment(s) failed to sync: {}",
            failures.len(),
            failures.join(", ")
        );
    }

    Ok(())
}

pub fn run_with_runner(
    config: &Config,
    opts: &SyncOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let env = opts.env.expect("sync requires an environment");
    config.validate_env(env)?;
    let only = opts.only;
    let dry_run = opts.dry_run;
    let bail_on_err = opts.bail;
    let force = opts.force;
    let auto_deploy = opts.auto_deploy;
    let prefer = opts.prefer;

    if config.remotes.is_empty() {
        bail!("no remotes configured in esk.yaml");
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;

    let all_remotes = remotes::build_remotes(config, runner);

    if all_remotes.is_empty() {
        if config.remotes.is_empty() {
            bail!("no remotes configured in esk.yaml");
        }
        cliclack::log::warning(
            "No remotes available after preflight checks. Fix the issues above and try again.",
        )?;
        return Ok(());
    }

    // Filter by --only if provided
    let target_remotes: Vec<_> = if let Some(name) = only {
        let remote_names: Vec<String> = all_remotes.iter().map(|p| p.name().to_string()).collect();
        let filtered: Vec<_> = all_remotes
            .into_iter()
            .filter(|p| p.name() == name)
            .collect();
        if filtered.is_empty() {
            bail!("{}", suggest::unknown_remote(name, &remote_names));
        }
        filtered
    } else {
        all_remotes
    };

    // Phase 1: Pull from all target remotes
    let mut pulls: Vec<PullResult> = Vec::new();
    let mut remote_data: Vec<(String, BTreeMap<String, String>, u64)> = Vec::new();
    let mut pull_failures: Vec<String> = Vec::new();

    for rem in &target_remotes {
        match rem.pull(config, env) {
            Ok(Some((secrets, version))) => {
                pulls.push(PullResult {
                    remote: rem.name().to_string(),
                    outcome: PullOutcome::Fetched {
                        version,
                        secret_count: secrets.len(),
                    },
                });
                remote_data.push((rem.name().to_string(), secrets, version));
            }
            Ok(None) => {
                pulls.push(PullResult {
                    remote: rem.name().to_string(),
                    outcome: PullOutcome::Empty,
                });
            }
            Err(_) => {
                pulls.push(PullResult {
                    remote: rem.name().to_string(),
                    outcome: PullOutcome::Failed,
                });
                pull_failures.push(rem.name().to_string());
            }
        }
    }

    if !pull_failures.is_empty() && bail_on_err {
        bail!(
            "{} remote(s) failed to respond: {}. Use without --bail to reconcile with partial data.",
            pull_failures.len(),
            pull_failures.join(", ")
        );
    }

    if remote_data.is_empty() {
        cliclack::log::info("No remote data found. Nothing to reconcile.")?;
        return Ok(());
    }

    // Phase 2: Multi-source reconciliation (interactive prompts stay here)
    let remotes_ref: Vec<(&str, &BTreeMap<String, String>, u64)> = remote_data
        .iter()
        .map(|(name, secrets, version)| (name.as_str(), secrets, *version))
        .collect();

    let result = match reconcile::reconcile_multi_with_jump_limit(
        &payload,
        &remotes_ref,
        Some(env),
        prefer,
        !force,
    ) {
        Ok(r) => r,
        Err(e) if reconcile::is_version_jump_error(&e) && !force => {
            if std::io::stdin().is_terminal() {
                cliclack::log::warning(format!("{e}"))?;
                let accept = cliclack::confirm(
                    "Accept the large version jump? This may indicate a compromised remote.",
                )
                .initial_value(false)
                .interact()?;
                if !accept {
                    bail!("Aborted by user. Use --force to bypass version jump protection.");
                }
            } else {
                bail!("{e}\nRun with --force to bypass version jump protection.");
            }
            reconcile::reconcile_multi_with_jump_limit(
                &payload,
                &remotes_ref,
                Some(env),
                prefer,
                false,
            )?
        }
        Err(e) => return Err(e),
    };

    // Phase 3: Compute reconcile outcome + execute merge/push
    let reconcile_outcome;
    let mut pushes: Vec<RemotePushResult> = Vec::new();

    if dry_run {
        // Dry-run: compute outcome, fake pushes for sources_to_update
        if result.local_changed {
            let label = env_version_label(&result.merged_payload, env);
            if result.has_drift {
                reconcile_outcome = ReconcileOutcome::DriftRepair {
                    version_label: label,
                };
            } else {
                reconcile_outcome = ReconcileOutcome::Merged {
                    version_label: label,
                };
            }
        } else if !result.sources_to_update.is_empty() {
            let label = env_version_label(&payload, env);
            if result.has_drift {
                reconcile_outcome = ReconcileOutcome::DriftRepair {
                    version_label: label,
                };
            } else {
                reconcile_outcome = ReconcileOutcome::UpToDate {
                    version_label: label,
                };
            }
        } else {
            let label = env_version_label(&payload, env);
            reconcile_outcome = ReconcileOutcome::UpToDate {
                version_label: label,
            };
        }

        // In dry-run, represent sources_to_update as push entries (rendered as "would push")
        for name in &result.sources_to_update {
            pushes.push(RemotePushResult {
                remote: name.clone(),
                success: true,
                error: None,
            });
        }
    } else {
        // Live mode: merge + push
        if result.local_changed {
            // Detect values that became empty from remote merge (skip allow_empty secrets)
            let resolved = config.resolve_secrets()?;
            let mut empty_from_remote: Vec<String> = Vec::new();
            for (composite, value) in &result.merged_payload.secrets {
                if crate::validate::is_effectively_empty(value) {
                    let bare_key = composite
                        .rsplit_once(':')
                        .map_or(composite.as_str(), |(k, _)| k);
                    let is_allowed = resolved.iter().any(|s| s.key == *bare_key && s.allow_empty);
                    if is_allowed {
                        continue;
                    }
                    let was_empty_locally = payload
                        .secrets
                        .get(composite)
                        .is_some_and(|v| crate::validate::is_effectively_empty(v));
                    if !was_empty_locally {
                        empty_from_remote.push(composite.clone());
                    }
                }
            }
            if !empty_from_remote.is_empty() {
                empty_from_remote.sort();
                cliclack::log::warning(format!(
                    "Remote introduced {} empty value{}: {}",
                    empty_from_remote.len(),
                    if empty_from_remote.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                    empty_from_remote.join(", ")
                ))?;
            }

            store.set_payload(&result.merged_payload)?;
            let label = env_version_label(&result.merged_payload, env);
            reconcile_outcome = ReconcileOutcome::Merged {
                version_label: label,
            };
        } else {
            let label = env_version_label(&payload, env);
            if result.has_drift {
                reconcile_outcome = ReconcileOutcome::DriftRepair {
                    version_label: label,
                };
            } else {
                reconcile_outcome = ReconcileOutcome::UpToDate {
                    version_label: label,
                };
            }
        }

        // Push merged/current result back to stale remotes
        if !result.sources_to_update.is_empty() {
            let updated_payload = if result.local_changed {
                &result.merged_payload
            } else {
                &payload
            };
            let sync_index_path = config.root.join(".esk/sync-index.json");
            let mut sync_index = SyncIndex::load(&sync_index_path);
            let pushed_version = updated_payload.env_version(env);

            for rem in &target_remotes {
                if !result.sources_to_update.iter().any(|s| s == rem.name()) {
                    continue;
                }
                match rem.push(updated_payload, config, env) {
                    Ok(()) => {
                        sync_index.record_success(rem.name(), env, pushed_version);
                        pushes.push(RemotePushResult {
                            remote: rem.name().to_string(),
                            success: true,
                            error: None,
                        });
                    }
                    Err(e) => {
                        sync_index.record_failure(rem.name(), env, pushed_version, e.to_string());
                        pushes.push(RemotePushResult {
                            remote: rem.name().to_string(),
                            success: false,
                            error: Some(e.to_string()),
                        });
                    }
                }
            }
            sync_index.save()?;
        }
    }

    // Phase 4: Render report
    let report = SyncReport {
        env: env.to_string(),
        pulls,
        reconcile: reconcile_outcome,
        pushes,
        dry_run,
    };

    report.render()?;

    if report.has_push_failures() {
        let fail_count = report.push_failure_count();
        bail!(
            "{fail_count} remote(s) failed to receive merged data. Run `esk sync --env {env}` to retry."
        );
    }

    // Auto-deploy stays after render
    if !dry_run && auto_deploy && result.local_changed {
        cliclack::log::step("Running deploy...")?;
        crate::cli::deploy::run_with_runner(
            config,
            &crate::cli::deploy::DeployOptions {
                env: Some(env),
                force: false,
                dry_run: false,
                verbose: false,
                skip_validation: false,
                bail: false,
                allow_empty: true,
                prune: false,
            },
            runner,
        )?;
    }

    Ok(())
}
