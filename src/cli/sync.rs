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
    pub strict: bool,
    pub force: bool,
    pub auto_deploy: bool,
    pub prefer: ConflictPreference,
}

const SYNC_LINE_WIDTH: usize = 30;

pub struct RemotePushResult {
    pub remote: String,
    pub outcome: Result<(), String>,
}

fn env_version_label(payload: &StorePayload, env: &str) -> String {
    ui::format_version_label(payload.env_version(env), payload.env_last_changed_at(env))
}

/// Format a pull result line for progressive rendering.
fn format_pull_line(name: &str, outcome: &PullOutcome) -> String {
    match outcome {
        PullOutcome::Fetched {
            version,
            secret_count,
        } => ui::format_dashboard_line(
            &format!("\u{2193} {name}"),
            &format!("v{version}, {secret_count} secrets"),
            SYNC_LINE_WIDTH,
        ),
        PullOutcome::Empty => ui::format_dashboard_line(
            &format!("\u{2193} {name}"),
            &style("no data").dim().to_string(),
            SYNC_LINE_WIDTH,
        ),
        PullOutcome::Failed(reason) => {
            let status = format!("{} \u{2014} {}", style("failed").red(), style(reason).dim());
            ui::format_dashboard_line(&format!("\u{2193} {name}"), &status, SYNC_LINE_WIDTH)
        }
    }
}

/// Format a push result line for progressive rendering.
fn format_push_line(name: &str, outcome: Result<(), &str>, dry_run: bool) -> String {
    if dry_run {
        ui::format_dashboard_line(
            &format!("\u{2191} {name}"),
            &style("would push").dim().to_string(),
            SYNC_LINE_WIDTH,
        )
    } else {
        match outcome {
            Ok(()) => ui::format_dashboard_line(
                &format!("\u{2191} {name}"),
                &style("synced").green().to_string(),
                SYNC_LINE_WIDTH,
            ),
            Err(reason) => {
                let status = format!("{} \u{2014} {}", style("failed").red(), style(reason).dim());
                ui::format_dashboard_line(&format!("\u{2191} {name}"), &status, SYNC_LINE_WIDTH)
            }
        }
    }
}

enum PullOutcome {
    Fetched { version: u64, secret_count: usize },
    Empty,
    Failed(String),
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
        spinner.start(format!("\u{2191} {}...", rem.name()));

        match rem.push(payload, config, env) {
            Ok(()) => {
                spinner.stop(format!(
                    "\u{2191} {}  {}",
                    rem.name(),
                    style("done").green()
                ));
                sync_index.record_success(rem.name(), env, pushed_version);
                results.push(RemotePushResult {
                    remote: rem.name().to_string(),
                    outcome: Ok(()),
                });
            }
            Err(e) => {
                spinner.error(format!(
                    "\u{2191} {}  {} \u{2014} {e}",
                    rem.name(),
                    style("failed").red()
                ));
                sync_index.record_failure(rem.name(), env, pushed_version, e.to_string());
                results.push(RemotePushResult {
                    remote: rem.name().to_string(),
                    outcome: Err(e.to_string()),
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

    let version = SecretStore::open(&config.root)?.payload()?.version;
    let remote_count = config.remotes.len();
    let scope = match options.env {
        Some(e) => format!(
            "{} remote{} · {}",
            remote_count,
            if remote_count == 1 { "" } else { "s" },
            e
        ),
        None => format!(
            "{} remote{}",
            remote_count,
            if remote_count == 1 { "" } else { "s" }
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
            if options.strict {
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
    let strict = opts.strict;
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

    // Phase 1: Parallel pull from all target remotes
    let remote_count = target_remotes.len();
    let spinner = cliclack::spinner();
    spinner.start(format!(
        "Pulling {} remote{}...",
        remote_count,
        if remote_count == 1 { "" } else { "s" }
    ));

    #[allow(clippy::type_complexity)]
    let pull_results: Vec<(String, Result<Option<(BTreeMap<String, String>, u64)>>)> =
        std::thread::scope(|s| {
            let handles: Vec<_> = target_remotes
                .iter()
                .map(|rem| {
                    let name = rem.name().to_string();
                    s.spawn(move || (name, rem.pull(config, env)))
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

    // Process pull results
    let mut pull_lines: Vec<String> = Vec::new();
    let mut remote_data: Vec<(String, BTreeMap<String, String>, u64)> = Vec::new();
    let mut pull_failures: Vec<String> = Vec::new();

    for (name, result) in pull_results {
        match result {
            Ok(Some((secrets, version))) => {
                let outcome = PullOutcome::Fetched {
                    version,
                    secret_count: secrets.len(),
                };
                pull_lines.push(format_pull_line(&name, &outcome));
                remote_data.push((name, secrets, version));
            }
            Ok(None) => {
                pull_lines.push(format_pull_line(&name, &PullOutcome::Empty));
                remote_data.push((name, BTreeMap::new(), 0));
            }
            Err(e) => {
                pull_lines.push(format_pull_line(&name, &PullOutcome::Failed(e.to_string())));
                pull_failures.push(name);
            }
        }
    }

    spinner.stop(format!(
        "Pulled {} remote{}",
        remote_count,
        if remote_count == 1 { "" } else { "s" }
    ));
    cliclack::log::step(pull_lines.join("\n"))?;

    if !pull_failures.is_empty() && strict {
        bail!(
            "{} remote(s) failed to respond: {}. Use without --strict to reconcile with partial data.",
            pull_failures.len(),
            pull_failures.join(", ")
        );
    }

    if remote_data.is_empty() {
        return Ok(());
    }

    // Phase 2: Reconcile (single-threaded, may prompt interactively)
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

    // Phase 3: Apply merge + parallel push
    let reconcile_status;
    let mut push_failure_count = 0usize;

    if dry_run {
        // Render reconcile status
        if result.local_changed {
            let label = env_version_label(&result.merged_payload, env);
            reconcile_status = if result.has_drift {
                format!(
                    "{} Current ({}), would repair drift",
                    ui::Icon::Merge,
                    label
                )
            } else {
                format!("{} Would merge \u{2192} {}", ui::Icon::Merge, label)
            };
        } else {
            let label = env_version_label(&payload, env);
            reconcile_status = if result.has_drift {
                format!(
                    "{} Current ({}), would repair drift",
                    ui::Icon::Merge,
                    label
                )
            } else {
                format!("{} Up to date \u{2192} {}", ui::Icon::Success, label)
            };
        }
        cliclack::log::info(reconcile_status)?;

        // Dry-run push lines
        if !result.sources_to_update.is_empty() {
            let push_lines: Vec<String> = result
                .sources_to_update
                .iter()
                .map(|name| format_push_line(name, Ok(()), true))
                .collect();
            cliclack::log::step(format!(
                "Would push {} remote{}\n{}",
                result.sources_to_update.len(),
                if result.sources_to_update.len() == 1 {
                    ""
                } else {
                    "s"
                },
                push_lines.join("\n")
            ))?;
        }
    } else {
        // Live: apply merge
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
            reconcile_status = format!("{} Merged \u{2192} {}", ui::Icon::Merge, label);
        } else {
            let label = env_version_label(&payload, env);
            reconcile_status = if result.has_drift {
                format!("{} Stale remotes (repairing...)", ui::Icon::Merge)
            } else {
                format!("{} Up to date \u{2192} {}", ui::Icon::Success, label)
            };
        }
        cliclack::log::info(reconcile_status)?;

        // Parallel push to stale remotes
        if !result.sources_to_update.is_empty() {
            let updated_payload = if result.local_changed {
                &result.merged_payload
            } else {
                &payload
            };

            let stale_remotes: Vec<&Box<dyn SyncRemote + '_>> = target_remotes
                .iter()
                .filter(|rem| result.sources_to_update.iter().any(|s| s == rem.name()))
                .collect();

            let push_count = stale_remotes.len();
            let push_spinner = cliclack::spinner();
            push_spinner.start(format!(
                "Pushing {} remote{}...",
                push_count,
                if push_count == 1 { "" } else { "s" }
            ));

            let push_results: Vec<(String, Result<()>)> = std::thread::scope(|s| {
                let handles: Vec<_> = stale_remotes
                    .iter()
                    .map(|rem| {
                        let name = rem.name().to_string();
                        s.spawn(move || (name, rem.push(updated_payload, config, env)))
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });

            push_spinner.stop(format!(
                "Pushed {} remote{}",
                push_count,
                if push_count == 1 { "" } else { "s" }
            ));

            // Update sync index sequentially on main thread
            let sync_index_path = config.root.join(".esk/sync-index.json");
            let mut sync_index = SyncIndex::load(&sync_index_path);
            let pushed_version = updated_payload.env_version(env);

            let mut push_lines: Vec<String> = Vec::new();
            for (name, res) in &push_results {
                match res {
                    Ok(()) => {
                        sync_index.record_success(name, env, pushed_version);
                        push_lines.push(format_push_line(name, Ok(()), false));
                    }
                    Err(e) => {
                        sync_index.record_failure(name, env, pushed_version, e.to_string());
                        push_lines.push(format_push_line(name, Err(&e.to_string()), false));
                        push_failure_count += 1;
                    }
                }
            }
            sync_index.save()?;
            cliclack::log::step(push_lines.join("\n"))?;
        }
    }

    if push_failure_count > 0 {
        bail!(
            "{push_failure_count} remote(s) failed to receive merged data. Run `esk sync --env {env}` to retry."
        );
    }

    // Auto-deploy after sync
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
                strict: false,
                allow_empty: true,
                prune: false,
            },
            runner,
        )?;
    }

    Ok(())
}
