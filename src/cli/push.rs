use anyhow::{bail, Result};
use console::style;

use crate::adapters::{CommandRunner, RealCommandRunner};
use crate::config::Config;
use crate::plugins;
use crate::store::SecretStore;

pub fn run(config: &Config, env: &str, only: Option<&str>) -> Result<()> {
    run_with_runner(config, env, only, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    env: &str,
    only: Option<&str>,
    runner: &dyn CommandRunner,
) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!(
            "unknown environment '{env}'. Valid: {}",
            config.environments.join(", ")
        );
    }

    if config.plugins.is_empty() {
        bail!("no plugins configured in lockbox.yaml");
    }

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;

    let all_plugins = plugins::build_plugins(config, runner);

    if all_plugins.is_empty() {
        if config.plugins.is_empty() {
            bail!("no plugins configured in lockbox.yaml");
        } else {
            println!("  No plugins available after preflight checks. Fix the issues above and try again.");
            return Ok(());
        }
    }

    // Filter by --only if provided
    let target_plugins: Vec<_> = if let Some(name) = only {
        let filtered: Vec<_> = all_plugins
            .into_iter()
            .filter(|p| p.name() == name)
            .collect();
        if filtered.is_empty() {
            bail!("unknown plugin '{name}'");
        }
        filtered
    } else {
        all_plugins
    };

    let mut success_count = 0u32;
    let mut fail_count = 0u32;

    for plugin in &target_plugins {
        print!("  {} → {}...", style("pushing").cyan(), plugin.name());

        match plugin.push(&payload, config, env) {
            Ok(()) => {
                println!(" {}", style("done").green());
                success_count += 1;
            }
            Err(e) => {
                println!(" {}", style("failed").red());
                eprintln!("  {e}");
                fail_count += 1;
            }
        }
    }

    println!(
        "\n  {} (v{})",
        if fail_count == 0 {
            format!("{} plugin(s) pushed", success_count)
        } else {
            format!("{} pushed, {} failed", success_count, fail_count)
        },
        payload.version
    );

    if fail_count > 0 {
        bail!("{fail_count} plugin push(es) failed");
    }

    Ok(())
}
