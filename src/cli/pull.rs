use anyhow::{bail, Context, Result};
use console::style;

use crate::adapters::onepassword::OnePasswordAdapter;
use crate::adapters::RealCommandRunner;
use crate::config::Config;
use crate::reconcile::{self, ReconcileAction};
use crate::store::SecretStore;

pub fn run(config: &Config, env: &str, auto_sync: bool) -> Result<()> {
    if !config.environments.contains(&env.to_string()) {
        bail!(
            "unknown environment '{env}'. Valid: {}",
            config.environments.join(", ")
        );
    }

    let op_config = config
        .adapters
        .onepassword
        .as_ref()
        .context("onepassword adapter not configured in lockbox.yaml")?;

    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;

    let runner = RealCommandRunner;
    let adapter = OnePasswordAdapter {
        config,
        adapter_config: op_config,
        runner: &runner,
    };

    let item_name = config.onepassword_item_name(env)?;
    println!("  {} from {}", style("pulling").cyan(), item_name);

    let (remote_secrets, remote_version) = match adapter.pull_item(env)? {
        Some(data) => data,
        None => {
            println!("  No 1Password item found for '{env}'. Nothing to pull.");
            return Ok(());
        }
    };

    println!(
        "  Remote: v{} ({} secrets), Local: v{} ({} secrets)",
        remote_version,
        remote_secrets.len(),
        payload.version,
        payload.secrets.len()
    );

    let result = reconcile::reconcile(&payload, &remote_secrets, remote_version, env);

    match result.action {
        ReconcileAction::PullRemote => {
            let merged = result
                .merged_payload
                .as_ref()
                .context("reconcile returned PullRemote without merged payload")?;
            store.write_payload(merged)?;

            if !result.pulled.is_empty() {
                println!(
                    "  {} pulled {} secrets: {}",
                    style("merged").green(),
                    result.pulled.len(),
                    result.pulled.join(", ")
                );
            }

            if !result.pushed.is_empty() {
                // Push local-only secrets back to 1Password
                println!(
                    "  {} {} local-only secrets back to 1Password",
                    style("pushing").cyan(),
                    result.pushed.len()
                );
                // Re-read payload after merge to get all secrets
                let updated = store.payload()?;
                let suffix = format!(":{env}");
                let env_secrets = updated
                    .secrets
                    .iter()
                    .filter_map(|(k, v)| {
                        k.strip_suffix(&suffix)
                            .map(|bare| (bare.to_string(), v.clone()))
                    })
                    .collect();
                adapter.push_item(env, &env_secrets, updated.version)?;
            }
        }
        ReconcileAction::PushLocal => {
            println!(
                "  Local is newer (v{} > v{}). Run `lockbox push --env {env}` to update 1Password.",
                payload.version, remote_version
            );
        }
        ReconcileAction::NoOp => {
            println!(
                "  {} already in sync (v{})",
                style("up to date").green(),
                payload.version
            );
        }
    }

    if auto_sync && matches!(result.action, ReconcileAction::PullRemote) {
        println!("\n  Running sync...");
        crate::cli::sync::run(config, Some(env), false, false, false)?;
    }

    Ok(())
}
