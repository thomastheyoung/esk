use anyhow::Result;
use console::style;

use crate::config::Config;
use crate::store::SecretStore;
use crate::tracker::{SyncIndex, SyncStatus};

pub fn run(config: &Config, env: Option<&str>) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let index_path = config.root.join(".lockbox/sync-index.json");
    let index = SyncIndex::load(&index_path);
    let resolved = config.resolve_secrets()?;

    // Build the set of configured adapter names to filter against
    let adapter_names: Vec<&str> = config.adapter_names();

    let mut has_output = false;

    for secret in &resolved {
        for target in &secret.targets {
            if let Some(filter_env) = env {
                if target.environment != filter_env {
                    continue;
                }
            }

            // Skip targets whose adapter isn't a sync adapter (e.g. plugin-only targets)
            if !adapter_names.contains(&target.adapter.as_str()) {
                continue;
            }

            let composite = format!("{}:{}", secret.key, target.environment);
            let value = payload.secrets.get(&composite);
            let tracker_key = SyncIndex::tracker_key(
                &secret.key,
                &target.adapter,
                target.app.as_deref(),
                &target.environment,
            );

            let status_str = match (value, index.records.get(&tracker_key)) {
                (None, _) => style("no value").dim().to_string(),
                (Some(_), None) => style("never synced").yellow().to_string(),
                (Some(v), Some(record)) => {
                    let current_hash = SyncIndex::hash_value(v);
                    if record.last_sync_status == SyncStatus::Failed {
                        let err = record.last_error.as_deref().unwrap_or("unknown error");
                        format!("{} ({})", style("failed").red(), err)
                    } else if current_hash != record.value_hash {
                        style("pending").yellow().to_string()
                    } else {
                        style("synced").green().to_string()
                    }
                }
            };

            println!(
                "  {}:{} → {}  {}",
                secret.key, target.environment, target, status_str
            );
            has_output = true;
        }
    }

    if !has_output {
        println!("  No sync targets configured.");
    }

    // Show store version
    println!("\n  Store version: {}", payload.version);

    Ok(())
}
