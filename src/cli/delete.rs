use anyhow::{bail, Result};

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::Config;
use crate::plugin_tracker::PluginIndex;
use crate::plugins;
use crate::store::SecretStore;
use crate::suggest;

pub fn run(config: &Config, key: &str, env: &str, no_sync: bool, strict: bool) -> Result<()> {
    run_with_runner(config, key, env, no_sync, strict, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    key: &str,
    env: &str,
    no_sync: bool,
    strict: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!("{}", suggest::unknown_env(env, &config.environments));
    }

    if config.find_secret(key).is_none() {
        cliclack::log::warning(format!("Secret '{}' is not defined in esk.yaml", key))?;
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.delete(key, env)?;

    cliclack::log::success(format!("Deleted {}:{} (v{})", key, env, payload.version))?;

    if no_sync {
        return Ok(());
    }

    // Auto-push to all configured plugins
    let mut plugin_failures = 0u32;
    if !config.plugins.is_empty() {
        let plugin_index_path = config.root.join(".esk/plugin-index.json");
        let mut plugin_index = PluginIndex::load(&plugin_index_path);
        let all_plugins = plugins::build_plugins(config, runner);
        plugin_failures =
            super::sync::push_to_plugins(&all_plugins, &payload, config, env, &mut plugin_index)?;
        plugin_index.save()?;

        if plugin_failures > 0 && strict {
            bail!(
                "{plugin_failures} plugin(s) failed to push (--strict). Adapter deploy skipped.\n\
                 Fix the plugin issue, then run:\n  \
                 esk sync --env {env}\n  \
                 esk deploy --env {env}"
            );
        }
    }

    // Auto-deploy adapters (env files regenerate without deleted key; individual adapters delete)
    crate::cli::deploy::run_with_runner(config, Some(env), false, false, false, runner)?;

    if plugin_failures > 0 {
        bail!("{plugin_failures} plugin(s) failed to push. Run `esk sync --env {env}` to retry.");
    }

    Ok(())
}
