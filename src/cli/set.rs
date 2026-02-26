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
    pub bail: bool,
    pub skip_validation: bool,
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
    let bail = opts.bail;
    let skip_validation = opts.skip_validation;

    config.validate_env(env)?;

    // If the key isn't in config, offer to register it
    if config.find_secret(key).is_none() {
        let config_path = config.root.join("esk.yaml");
        if let Some(group) = group {
            config::add_secret_to_config(&config_path, key, group)?;
            cliclack::log::success(format!("Added '{}' to esk.yaml under {}", key, group))?;
        } else if std::io::stdin().is_terminal() {
            let add = cliclack::confirm(format!("Secret '{}' is not in esk.yaml. Add it?", key))
                .initial_value(true)
                .interact()?;

            if add {
                let mut groups = config::secret_group_names(config);
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
                cliclack::log::success(format!(
                    "Added '{}' to esk.yaml under {}",
                    key, chosen_group
                ))?;
            } else {
                cliclack::log::warning(format!("Secret '{}' is not defined in esk.yaml", key))?;
            }
        } else {
            cliclack::log::warning(format!("Secret '{}' is not defined in esk.yaml", key))?;
        }
    }

    let secret_value = match value {
        Some(v) => v.to_string(),
        None => cliclack::password(format!("Value for {} ({})", key, env))
            .mask('*')
            .interact()?,
    };

    if secret_value.contains('\n') || secret_value.contains('\r') {
        cliclack::log::warning(
            "Secret contains newlines. Some targets (fly, supabase) cannot sync newline-containing values.",
        )?;
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

    cliclack::log::success(format!("Set {}:{} (v{})", key, env, payload.version))?;

    if no_sync {
        return Ok(());
    }

    // Auto-push to all configured remotes
    let mut remote_failures = 0u32;
    if !config.remotes.is_empty() {
        let sync_index_path = config.root.join(".esk/sync-index.json");
        let mut sync_index = SyncIndex::load(&sync_index_path);
        let all_remotes = remotes::build_remotes(config, runner);
        remote_failures =
            super::sync::push_to_remotes(&all_remotes, &payload, config, env, &mut sync_index)?;
        sync_index.save()?;

        if remote_failures > 0 && bail {
            bail!(
                "{remote_failures} remote(s) failed to push (--bail). Target deploy skipped.\n\
                 Fix the remote issue, then run:\n  \
                 esk sync --env {env}\n  \
                 esk deploy --env {env}"
            );
        }
    }

    // Auto-deploy affected targets (skip validation — already validated above)
    // skip_requirements: user may be setting secrets incrementally
    crate::cli::deploy::run_with_runner(
        config,
        &crate::cli::deploy::DeployOptions {
            env: Some(env),
            force: false,
            dry_run: false,
            verbose: false,
            skip_validation: true,
            skip_requirements: true,
        },
        runner,
    )?;

    if remote_failures > 0 {
        bail!("{remote_failures} remote(s) failed to push. Run `esk sync --env {env}` to retry.");
    }

    Ok(())
}
