use anyhow::{bail, Context, Result};
use console::style;
use std::collections::BTreeMap;

use crate::adapters::onepass::OnePasswordAdapter;
use crate::config::Config;
use crate::store::SecretStore;

pub fn run(config: &Config, env: &str) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!("unknown environment '{env}'. Valid: {}", config.environments.join(", "));
    }

    let op_config = config
        .adapters
        .onepassword
        .as_ref()
        .context("onepassword adapter not configured in lockbox.yaml")?;

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;

    // Collect secrets for this environment (bare keys, not composite)
    let suffix = format!(":{env}");
    let env_secrets: BTreeMap<String, String> = payload
        .secrets
        .iter()
        .filter_map(|(k, v)| {
            k.strip_suffix(&suffix)
                .map(|bare| (bare.to_string(), v.clone()))
        })
        .collect();

    if env_secrets.is_empty() {
        println!("  No secrets for environment '{env}'.");
        return Ok(());
    }

    let adapter = OnePasswordAdapter {
        config,
        adapter_config: op_config,
    };

    let item_name = config.onepassword_item_name(env)?;
    println!(
        "  {} {} secrets → {} (v{})",
        style("pushing").cyan(),
        env_secrets.len(),
        item_name,
        payload.version
    );

    adapter.push_item(env, &env_secrets, payload.version)?;

    println!(
        "  {} {} secrets pushed to 1Password",
        style("done").green(),
        env_secrets.len()
    );

    Ok(())
}
