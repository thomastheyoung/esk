use std::io::IsTerminal;

use anyhow::{bail, Result};

use crate::config::Config;
use crate::remotes;
use crate::store::SecretStore;
use crate::sync_tracker::SyncIndex;
use crate::targets::{CommandRunner, RealCommandRunner};

pub fn run(config: &Config, key: &str, env: &str, no_sync: bool, bail: bool) -> Result<()> {
    run_with_runner(config, key, env, no_sync, bail, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    key: &str,
    env: &str,
    no_sync: bool,
    bail: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    config.validate_env(env)?;

    if config.find_secret(key).is_none() {
        cliclack::log::warning(format!("Secret '{}' is not defined in esk.yaml", key))?;
    }

    // Warn if deleting a required secret
    if let Some((_, def)) = config.find_secret(key) {
        if def.required.is_required_in(env) && std::io::stdin().is_terminal() {
            let targets: Vec<String> = def.targets.keys().map(|t| t.to_string()).collect();
            let target_list = if targets.is_empty() {
                String::new()
            } else {
                format!(" (targets: {})", targets.join(", "))
            };
            let confirm = cliclack::confirm(format!(
                "{}:{} is required{}. Delete anyway?",
                key, env, target_list,
            ))
            .initial_value(false)
            .interact()?;
            if !confirm {
                cliclack::log::info("Cancelled.")?;
                return Ok(());
            }
        }
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.delete(key, env)?;

    cliclack::log::success(format!("Deleted {}:{} (v{})", key, env, payload.version))?;

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

    // Auto-deploy targets (env files regenerate without deleted key; individual targets delete)
    // skip_requirements: the user intentionally deleted this secret
    crate::cli::deploy::run_with_runner(
        config,
        &crate::cli::deploy::DeployOptions {
            env: Some(env),
            force: false,
            dry_run: false,
            verbose: false,
            skip_validation: false,
            skip_requirements: true,
        },
        runner,
    )?;

    if remote_failures > 0 {
        bail!("{remote_failures} remote(s) failed to push. Run `esk sync --env {env}` to retry.");
    }

    Ok(())
}
