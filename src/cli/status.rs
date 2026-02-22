use anyhow::Result;
use console::style;
use std::collections::BTreeMap;

use crate::cli::GroupBy;
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

pub fn run(config: &Config, env: Option<&str>, all: bool, group_by: GroupBy) -> Result<()> {
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

    match group_by {
        GroupBy::Status => render_by_status(&entries, all)?,
        GroupBy::Env => render_by_group(&entries, all, |e| e.env.clone(), line_for_env)?,
        GroupBy::Target => render_by_group(&entries, all, |e| e.target.clone(), line_for_target)?,
        GroupBy::Key => render_by_group(&entries, all, |e| e.key.clone(), line_for_key)?,
    }

    print_plugin_status(config, env, payload.version)?;
    cliclack::log::info(format!("Store version: {}", payload.version))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn status_icon(status: &Status) -> &'static str {
    match status {
        Status::Failed => "✗",
        Status::Pending => "●",
        Status::Unset => "○",
        Status::Synced => "✓",
    }
}

fn styled_icon(status: &Status) -> console::StyledObject<&'static str> {
    match status {
        Status::Failed => style("✗").red(),
        Status::Pending => style("●").yellow(),
        Status::Unset => style("○").dim(),
        Status::Synced => style("✓").green(),
    }
}

fn status_label(status: &Status) -> &'static str {
    match status {
        Status::Failed => "failed",
        Status::Pending => "pending",
        Status::Unset => "unset",
        Status::Synced => "synced",
    }
}

fn worst_status<'a>(entries: impl Iterator<Item = &'a Entry>) -> Status {
    entries
        .map(|e| &e.status)
        .min()
        .cloned()
        .unwrap_or(Status::Synced)
}

fn count_summary(entries: &[&Entry]) -> String {
    let mut counts: BTreeMap<&Status, usize> = BTreeMap::new();
    for e in entries {
        *counts.entry(&e.status).or_default() += 1;
    }
    let parts: Vec<String> = counts
        .iter()
        .map(|(s, n)| format!("{n} {}", status_label(s)))
        .collect();
    parts.join(", ")
}

fn log_by_status(status: &Status, body: String) -> Result<()> {
    match status {
        Status::Failed => cliclack::log::error(body)?,
        Status::Pending => cliclack::log::warning(body)?,
        Status::Unset => cliclack::log::remark(body)?,
        Status::Synced => cliclack::log::success(body)?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Group-by-status rendering (original behavior)
// ---------------------------------------------------------------------------

fn render_by_status(entries: &[Entry], all: bool) -> Result<()> {
    let mut by_status: BTreeMap<Status, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        by_status
            .entry(entry.status.clone())
            .or_default()
            .push(entry);
    }

    let problem_count = entries.iter().filter(|e| e.status != Status::Synced).count();
    let synced_count = entries.iter().filter(|e| e.status == Status::Synced).count();

    if problem_count == 0 {
        cliclack::log::success(format!(
            "All {} targets synced",
            style(synced_count).bold()
        ))?;
        if all {
            print_status_group(&by_status, &Status::Synced)?;
        }
        return Ok(());
    }

    for status in &[Status::Failed, Status::Pending, Status::Unset] {
        print_status_group(&by_status, status)?;
    }

    if synced_count > 0 {
        if all {
            print_status_group(&by_status, &Status::Synced)?;
        } else {
            cliclack::log::success(format!(
                "{} synced  {}",
                style(synced_count).bold(),
                style("(use --all to show)").dim()
            ))?;
        }
    }

    Ok(())
}

fn print_status_group(by_status: &BTreeMap<Status, Vec<&Entry>>, status: &Status) -> Result<()> {
    let entries = match by_status.get(status) {
        Some(e) if !e.is_empty() => e,
        _ => return Ok(()),
    };

    let count_style = match status {
        Status::Failed => style(entries.len()).red().bold(),
        Status::Pending => style(entries.len()).yellow().bold(),
        Status::Unset => style(entries.len()).dim().bold(),
        Status::Synced => style(entries.len()).green().bold(),
    };

    let icon = status_icon(status);
    let label = status_label(status);
    let grouped = collapse_by_key_env(entries);

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

    log_by_status(status, format!("{header}\n{}", lines.join("\n")))?;
    Ok(())
}

/// Collapse entries by (key, env), collecting targets. Used by status grouping.
fn collapse_by_key_env<'a>(
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

// ---------------------------------------------------------------------------
// Generic group-by rendering (env, target, key)
// ---------------------------------------------------------------------------

fn render_by_group(
    entries: &[Entry],
    all: bool,
    group_key_fn: impl Fn(&Entry) -> String,
    line_fn: impl Fn(&Entry) -> String,
) -> Result<()> {
    let total = entries.len();
    let synced_count = entries.iter().filter(|e| e.status == Status::Synced).count();

    if synced_count == total {
        cliclack::log::success(format!(
            "All {} targets synced",
            style(synced_count).bold()
        ))?;
        if !all {
            return Ok(());
        }
    }

    // Group entries by the chosen key
    let mut groups: BTreeMap<String, Vec<&Entry>> = BTreeMap::new();
    for entry in entries {
        groups
            .entry(group_key_fn(entry))
            .or_default()
            .push(entry);
    }

    let mut collapsed_synced = 0usize;

    for (group_name, group_entries) in &groups {
        let visible: Vec<&&Entry> = if all {
            group_entries.iter().collect()
        } else {
            group_entries
                .iter()
                .filter(|e| e.status != Status::Synced)
                .collect()
        };

        if visible.is_empty() {
            collapsed_synced += group_entries.len();
            continue;
        }

        let worst = worst_status(group_entries.iter().copied());
        let summary = count_summary(group_entries);
        let icon = status_icon(&worst);
        let header = format!("{icon}  {}  {}", style(group_name).bold(), style(summary).dim());

        let lines: Vec<String> = visible.iter().map(|e| line_fn(e)).collect();
        let body = format!("{header}\n{}", lines.join("\n"));
        log_by_status(&worst, body)?;
    }

    if !all && collapsed_synced > 0 {
        cliclack::log::success(format!(
            "{} synced  {}",
            style(collapsed_synced).bold(),
            style("(use --all to show)").dim()
        ))?;
    }

    Ok(())
}

// Line formatters for each grouping mode

fn line_for_env(entry: &Entry) -> String {
    let icon = styled_icon(&entry.status);
    let mut line = format!(
        "  {icon} {}  → {}",
        style(&entry.key).dim(),
        &entry.target
    );
    if let Some(err) = &entry.error {
        line.push_str(&format!("  {}", style(format!("({err})")).dim()));
    }
    line
}

fn line_for_target(entry: &Entry) -> String {
    let icon = styled_icon(&entry.status);
    let mut line = format!(
        "  {icon} {}",
        style(format!("{}:{}", entry.key, entry.env)).dim(),
    );
    if let Some(err) = &entry.error {
        line.push_str(&format!("  {}", style(format!("({err})")).dim()));
    }
    line
}

fn line_for_key(entry: &Entry) -> String {
    let icon = styled_icon(&entry.status);
    let mut line = format!(
        "  {icon} {}  → {}",
        style(&entry.env).dim(),
        &entry.target
    );
    if let Some(err) = &entry.error {
        line.push_str(&format!("  {}", style(format!("({err})")).dim()));
    }
    line
}

// ---------------------------------------------------------------------------
// Shared utilities
// ---------------------------------------------------------------------------

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
