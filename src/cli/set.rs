use anyhow::{bail, Result};
use std::io::IsTerminal;

use crate::config::{self, Config};
use crate::remotes;
use crate::store::SecretStore;
use crate::sync_tracker::SyncIndex;
use crate::targets::{CommandRunner, RealCommandRunner};

pub struct SetOptions<'a> {
    pub key: &'a str,
    pub env: &'a str,
    pub value: Option<&'a str>,
    pub group: Option<&'a str>,
    pub no_sync: bool,
    pub strict: bool,
    pub skip_validation: bool,
    pub force: bool,
}

struct SetReport {
    key: String,
    env: String,
    version: u64,
    push_results: Vec<super::sync::RemotePushResult>,
}

impl SetReport {
    fn remote_failure_count(&self) -> usize {
        self.push_results
            .iter()
            .filter(|r| r.outcome.is_err())
            .count()
    }

    fn render(&self) -> Result<()> {
        cliclack::log::success(format!("Set {}:{} (v{})", self.key, self.env, self.version))?;
        Ok(())
    }
}

pub fn run(config: &Config, opts: &SetOptions<'_>) -> Result<()> {
    run_with_runner(config, opts, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    opts: &SetOptions<'_>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let key = opts.key;
    let env = opts.env;
    let value = opts.value;
    let group = opts.group;
    let no_sync = opts.no_sync;
    let strict = opts.strict;
    let skip_validation = opts.skip_validation;

    config.validate_env(env)?;

    // If the key isn't in config, offer to register it
    if config.find_secret(key).is_none() {
        let config_path = config.root.join("esk.yaml");
        if let Some(group) = group {
            config::add_secret_to_config(&config_path, key, group)?;
            cliclack::log::success(format!("Added '{key}' to esk.yaml under {group}"))?;
        } else if std::io::stdin().is_terminal() {
            let add = cliclack::confirm(format!("Secret '{key}' is not in esk.yaml. Add it?"))
                .initial_value(true)
                .interact()?;

            if add {
                let mut groups = config.secret_group_names();
                let has_groups = !groups.is_empty();

                let chosen_group = if has_groups {
                    groups.push("(New group)".to_string());
                    let options: Vec<(&str, &str, &str)> = groups
                        .iter()
                        .map(|g| (g.as_str(), g.as_str(), ""))
                        .collect();
                    let selected: &str = cliclack::select("Which group?")
                        .items(&options)
                        .interact()?;

                    if selected == "(New group)" {
                        let name: String = cliclack::input("Group name:").interact()?;
                        name
                    } else {
                        selected.to_string()
                    }
                } else {
                    let name: String = cliclack::input("Group name:").interact()?;
                    name
                };

                config::add_secret_to_config(&config_path, key, &chosen_group)?;
                cliclack::log::success(format!("Added '{key}' to esk.yaml under {chosen_group}"))?;
            } else {
                cliclack::log::warning(format!("Secret '{key}' is not defined in esk.yaml"))?;
            }
        } else {
            cliclack::log::warning(format!("Secret '{key}' is not defined in esk.yaml"))?;
        }
    }

    let secret_value = match value {
        Some(v) => v.to_string(),
        None => cliclack::password(format!("Value for {key} ({env})"))
            .mask('*')
            .interact()?,
    };

    if secret_value.contains('\n') || secret_value.contains('\r') {
        cliclack::log::warning(
            "Secret contains newlines. Some targets (fly, supabase) cannot sync newline-containing values.",
        )?;
    }

    if !opts.force && crate::validate::is_effectively_empty(&secret_value) {
        let allow = config.find_secret(key).is_some_and(|(_, d)| d.allow_empty);
        if !allow {
            let kind = if secret_value.is_empty() {
                "empty"
            } else {
                "whitespace-only"
            };
            if std::io::stdin().is_terminal() {
                cliclack::log::warning(format!("Value for {key}:{env} is {kind}"))?;
                let proceed = cliclack::confirm(
                    "Empty values can break defaults and type coercion. Continue?",
                )
                .initial_value(false)
                .interact()?;
                if !proceed {
                    bail!("Aborted. Use --force to bypass.");
                }
            } else {
                cliclack::log::warning(format!("Setting {kind} value for {key}:{env}"))?;
            }
        }
    }

    if !skip_validation {
        if let Some((_, def)) = config.find_secret(key) {
            if let Some(ref spec) = def.validate {
                crate::validate::validate_value(key, &secret_value, spec).map_err(|e| {
                    anyhow::anyhow!(
                        "Validation failed for {key}: {e}\n  Use --skip-validation to bypass"
                    )
                })?;
            }
        }
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.set(key, env, &secret_value)?;

    let mut push_results = Vec::new();
    if !no_sync && !config.remotes.is_empty() {
        let sync_index_path = config.root.join(".esk/sync-index.json");
        let mut sync_index = SyncIndex::load(&sync_index_path);
        let all_remotes = remotes::build_remotes(config, runner);
        push_results =
            super::sync::push_to_remotes(&all_remotes, &payload, config, env, &mut sync_index)?;
        sync_index.save()?;
    }

    let report = SetReport {
        key: key.to_string(),
        env: env.to_string(),
        version: payload.version,
        push_results,
    };

    report.render()?;

    if no_sync {
        return Ok(());
    }

    let remote_failures = report.remote_failure_count();
    if remote_failures > 0 && strict {
        bail!(
            "{remote_failures} remote(s) failed to push (--strict). Target deploy skipped.\n\
             Fix the remote issue, then run:\n  \
             esk sync --env {env}\n  \
             esk deploy --env {env}"
        );
    }

    // Auto-deploy affected targets (skip validation — already validated above)
    // strict: false — user may be setting secrets incrementally
    // allow_empty: user already confirmed at set time, don't double-prompt
    crate::cli::deploy::run_with_runner(
        config,
        &crate::cli::deploy::DeployOptions {
            env: Some(env),
            force: false,
            dry_run: false,
            verbose: false,
            skip_validation: true,
            strict: false,
            allow_empty: true,
            prune: false,
        },
        runner,
    )?;

    if remote_failures > 0 {
        bail!("{remote_failures} remote(s) failed to push. Run `esk sync --env {env}` to retry.");
    }

    Ok(())
}
