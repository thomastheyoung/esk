use anyhow::Result;
use console::style;

use crate::config::Config;
use crate::store::SecretStore;

pub fn run(config: &Config, env: Option<&str>) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let all_secrets = store.list()?;

    if all_secrets.is_empty() {
        println!("  No secrets stored. Run `lockbox set <KEY> --env <ENV>` to add one.");
        return Ok(());
    }

    // Group by vendor using config, then show which envs have values
    // Also collect any secrets in the store that aren't in config
    let mut shown_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for (vendor, vendor_secrets) in &config.secrets {
        let mut vendor_entries: Vec<(&str, Vec<String>)> = Vec::new();

        for key in vendor_secrets.keys() {
            shown_keys.insert(key.clone());
            let mut envs_with_values = Vec::new();
            for e in &config.environments {
                if let Some(filter) = env {
                    if e != filter {
                        continue;
                    }
                }
                let composite = format!("{key}:{e}");
                if all_secrets.contains_key(&composite) {
                    envs_with_values.push(e.clone());
                }
            }
            vendor_entries.push((key, envs_with_values));
        }

        if vendor_entries.is_empty() {
            continue;
        }

        println!("\n  {}", style(vendor).bold().underlined());
        for (key, envs) in vendor_entries {
            if envs.is_empty() {
                println!("    {} {}", style(key).dim(), style("(no values)").dim());
            } else {
                let env_list = envs
                    .iter()
                    .map(|e| style(e).green().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                println!("    {key}  [{env_list}]");
            }
        }
    }

    // Show uncategorized secrets (in store but not in config)
    let mut uncategorized: Vec<(String, String)> = Vec::new();
    for composite_key in all_secrets.keys() {
        if let Some((key, _env)) = composite_key.rsplit_once(':') {
            if !shown_keys.contains(key) {
                uncategorized.push((key.to_string(), composite_key.clone()));
            }
        }
    }

    if !uncategorized.is_empty() {
        println!(
            "\n  {}",
            style("Uncategorized (not in lockbox.yaml)").bold().underlined()
        );
        for (key, composite) in &uncategorized {
            println!("    {} {}", key, style(&composite).dim());
        }
    }

    println!();
    Ok(())
}
