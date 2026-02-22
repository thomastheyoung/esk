use anyhow::Result;
use console::style;
use std::collections::BTreeMap;

use crate::config::{Config, ResolvedTarget};
use crate::plugin_tracker::{PluginIndex, PushStatus};
use crate::store::SecretStore;
use crate::tracker::{SyncIndex, SyncStatus};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Failed,
    Pending,
    Unset,
    Synced,
}

struct Entry {
    key: String,
    env: String,
    target: String,
    status: Status,
    error: Option<String>,
}

pub fn run(config: &Config, env: Option<&str>, all: bool) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let payload = store.payload()?;
    let index_path = config.root.join(".lockbox/sync-index.json");
    let index = SyncIndex::load(&index_path);
    let resolved = config.resolve_secrets()?;
    let adapter_names: Vec<&str> = config.adapter_names();

    let mut entries: Vec<Entry> = Vec::new();

    for secret in &resolved {
        for target in &secret.targets {
            if let Some(filter_env) = env {
                if target.environment != filter_env {
                    continue;
                }
            }
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

            let (status, error) = match (value, index.records.get(&tracker_key)) {
                (None, _) => (Status::Unset, None),
                (Some(_), None) => (Status::Pending, None),
                (Some(v), Some(record)) => {
                    let current_hash = SyncIndex::hash_value(v);
                    if record.last_sync_status == SyncStatus::Failed {
                        let err = record
                            .last_error
                            .as_deref()
                            .unwrap_or("unknown error")
                            .to_string();
                        (Status::Failed, Some(err))
                    } else if current_hash != record.value_hash {
                        (Status::Pending, None)
                    } else {
                        (Status::Synced, None)
                    }
                }
            };

            entries.push(Entry {
                key: secret.key.clone(),
                env: target.environment.clone(),
                target: format_target(target),
                status,
                error,
            });
        }
    }

    if entries.is_empty() {
        cliclack::log::info("No sync targets configured.")?;
        print_plugin_status(config, env, payload.version)?;
        cliclack::log::info(format!("Store version: {}", payload.version))?;
        return Ok(());
    }

    // Group by status
    let mut by_status: BTreeMap<Status, Vec<&Entry>> = BTreeMap::new();
    for entry in &entries {
        by_status.entry(entry.status.clone()).or_default().push(entry);
    }

    let problem_count = entries.iter().filter(|e| e.status != Status::Synced).count();
    let synced_count = entries.iter().filter(|e| e.status == Status::Synced).count();

    if problem_count == 0 {
        cliclack::log::success(format!(
            "All {} targets synced",
            style(synced_count).bold()
        ))?;
        if all {
            print_group(&by_status, &Status::Synced)?;
        }
        print_plugin_status(config, env, payload.version)?;
        cliclack::log::info(format!("Store version: {}", payload.version))?;
        return Ok(());
    }

    // Print problem groups in severity order
    for status in &[Status::Failed, Status::Pending, Status::Unset] {
        print_group(&by_status, status)?;
    }

    // Synced summary or expanded list
    if synced_count > 0 {
        if all {
            print_group(&by_status, &Status::Synced)?;
        } else {
            cliclack::log::success(format!(
                "{} synced  {}",
                style(synced_count).bold(),
                style("(use --all to show)").dim()
            ))?;
        }
    }

    print_plugin_status(config, env, payload.version)?;
    cliclack::log::info(format!("Store version: {}", payload.version))?;

    Ok(())
}

fn print_group(by_status: &BTreeMap<Status, Vec<&Entry>>, status: &Status) -> Result<()> {
    let entries = match by_status.get(status) {
        Some(e) if !e.is_empty() => e,
        _ => return Ok(()),
    };

    let (icon, label, count_style) = match status {
        Status::Failed => ("✗", "failed", style(entries.len()).red().bold()),
        Status::Pending => ("●", "pending", style(entries.len()).yellow().bold()),
        Status::Unset => ("○", "unset", style(entries.len()).dim().bold()),
        Status::Synced => ("✓", "synced", style(entries.len()).green().bold()),
    };

    // Group entries: key:env → [targets], collapsing same key+env+status
    let grouped = group_entries(entries);

    let header = format!("{icon} {count_style} {label}");

    let lines: Vec<String> = grouped
        .iter()
        .map(|(key, env, targets, error)| {
            let targets_str = targets.join(", ");
            let mut line = format!(
                "  {}  → {}",
                style(format!("{key}:{env}")).dim(),
                targets_str
            );
            if let Some(err) = error {
                line.push_str(&format!("  {}", style(format!("({err})")).dim()));
            }
            line
        })
        .collect();

    match status {
        Status::Failed => cliclack::log::error(format!("{header}\n{}", lines.join("\n")))?,
        Status::Pending => cliclack::log::warning(format!("{header}\n{}", lines.join("\n")))?,
        Status::Unset => cliclack::log::remark(format!("{header}\n{}", lines.join("\n")))?,
        Status::Synced => cliclack::log::success(format!("{header}\n{}", lines.join("\n")))?,
    }

    Ok(())
}

/// Group entries by (key, env), collecting targets into a list.
/// Returns Vec<(key, env, targets, error)>.
fn group_entries<'a>(
    entries: &[&'a Entry],
) -> Vec<(&'a str, &'a str, Vec<&'a str>, Option<&'a str>)> {
    let mut map: BTreeMap<(&str, &str), (Vec<&str>, Option<&str>)> = BTreeMap::new();
    for entry in entries {
        let group = map
            .entry((&entry.key, &entry.env))
            .or_insert_with(|| (Vec::new(), None));
        group.0.push(&entry.target);
        if entry.error.is_some() {
            group.1 = entry.error.as_deref();
        }
    }
    map.into_iter()
        .map(|((key, env), (targets, error))| (key, env, targets, error))
        .collect()
}

fn format_target(target: &ResolvedTarget) -> String {
    let mut s = target.adapter.clone();
    if let Some(app) = &target.app {
        s.push(':');
        s.push_str(app);
    }
    s
}

fn print_plugin_status(config: &Config, env: Option<&str>, store_version: u64) -> Result<()> {
    if config.plugins.is_empty() {
        return Ok(());
    }

    let plugin_index_path = config.root.join(".lockbox/plugin-index.json");
    let plugin_index = PluginIndex::load(&plugin_index_path);

    let plugin_names: Vec<&String> = config.plugins.keys().collect();
    let envs: Vec<&str> = match env {
        Some(e) => vec![e],
        None => config.environments.iter().map(|s| s.as_str()).collect(),
    };

    let mut lines: Vec<String> = Vec::new();

    for plugin_name in &plugin_names {
        for env_name in &envs {
            let key = PluginIndex::tracker_key(plugin_name, env_name);
            let line = match plugin_index.records.get(&key) {
                Some(record) if record.last_push_status == PushStatus::Failed => {
                    let err = record
                        .last_error
                        .as_deref()
                        .unwrap_or("unknown error");
                    format!(
                        "  {} {}  {} {}",
                        style("✗").red(),
                        style(format!("{plugin_name}:{env_name}")).dim(),
                        style(format!("v{}", record.pushed_version)).dim(),
                        style(format!("({err})")).dim()
                    )
                }
                Some(record) if record.pushed_version >= store_version => {
                    format!(
                        "  {} {}  {}",
                        style("✓").green(),
                        style(format!("{plugin_name}:{env_name}")).dim(),
                        style(format!("v{}", record.pushed_version)).dim()
                    )
                }
                Some(record) => {
                    format!(
                        "  {} {}  {}",
                        style("●").yellow(),
                        style(format!("{plugin_name}:{env_name}")).dim(),
                        style(format!(
                            "v{} (local is v{})",
                            record.pushed_version, store_version
                        ))
                        .dim()
                    )
                }
                None => {
                    format!(
                        "  {} {}  {}",
                        style("○").dim(),
                        style(format!("{plugin_name}:{env_name}")).dim(),
                        style("never pushed").dim()
                    )
                }
            };
            lines.push(line);
        }
    }

    if !lines.is_empty() {
        cliclack::log::info(format!("Plugins\n{}", lines.join("\n")))?;
    }

    Ok(())
}
