use anyhow::{bail, Result};
use console::style;
use std::collections::BTreeMap;

use crate::adapters::RealCommandRunner;
use crate::config::Config;
use crate::plugins;
use crate::reconcile;
use crate::store::SecretStore;

pub fn run(config: &Config, env: &str, only: Option<&str>, auto_sync: bool) -> Result<()> {
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

    let runner = RealCommandRunner;
    let all_plugins = plugins::build_plugins(config, &runner);

    if all_plugins.is_empty() {
        bail!("no plugins configured in lockbox.yaml");
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

    // Pull from all target plugins
    let mut remote_data: Vec<(String, BTreeMap<String, String>, u64)> = Vec::new();

    for plugin in &target_plugins {
        print!("  {} ← {}...", style("pulling").cyan(), plugin.name());

        match plugin.pull(config, env) {
            Ok(Some((secrets, version))) => {
                println!(
                    " {} (v{}, {} secrets)",
                    style("ok").green(),
                    version,
                    secrets.len()
                );
                remote_data.push((plugin.name().to_string(), secrets, version));
            }
            Ok(None) => {
                println!(" {}", style("no data").dim());
            }
            Err(e) => {
                println!(" {}", style("failed").red());
                eprintln!("  {e}");
            }
        }
    }

    if remote_data.is_empty() {
        println!("\n  No remote data found. Nothing to reconcile.");
        return Ok(());
    }

    // Multi-source reconciliation
    let remotes: Vec<(&str, &BTreeMap<String, String>, u64)> = remote_data
        .iter()
        .map(|(name, secrets, version)| (name.as_str(), secrets, *version))
        .collect();

    let result = reconcile::reconcile_multi(&payload, &remotes);

    if result.local_changed {
        store.write_payload(&result.merged_payload)?;
        println!(
            "\n  {} local store updated to v{}",
            style("merged").green(),
            result.merged_payload.version
        );

        // Push merged result back to plugins that were behind
        if !result.sources_to_update.is_empty() {
            let updated_payload = store.payload()?;
            for plugin in &target_plugins {
                if result
                    .sources_to_update
                    .contains(&plugin.name().to_string())
                {
                    print!(
                        "  {} → {}...",
                        style("pushing merged").cyan(),
                        plugin.name()
                    );
                    match plugin.push(&updated_payload, config, env) {
                        Ok(()) => println!(" {}", style("done").green()),
                        Err(e) => {
                            println!(" {}", style("failed").red());
                            eprintln!("  {e}");
                        }
                    }
                }
            }
        }
    } else {
        println!(
            "\n  {} already in sync (v{})",
            style("up to date").green(),
            payload.version
        );
    }

    if auto_sync && result.local_changed {
        println!("\n  Running sync...");
        crate::cli::sync::run(config, Some(env), false, false, false)?;
    }

    Ok(())
}
