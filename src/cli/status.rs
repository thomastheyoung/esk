use anyhow::Result;
use chrono::Utc;
use console::style;
use std::collections::BTreeSet;

use crate::adapters::{check_adapter_health, AdapterHealth, CommandRunner, RealCommandRunner};
use crate::config::{Config, ResolvedTarget};
use crate::plugin_tracker::{PluginIndex, PushStatus};
use crate::plugins::{check_plugin_health, PluginHealth};
use crate::store::SecretStore;
use crate::tracker::{SyncIndex, SyncStatus};

// ---------------------------------------------------------------------------
// Custom theme (same as list.rs — prevents dim from clobbering inline ANSI)
// ---------------------------------------------------------------------------

struct StatusTheme;

impl cliclack::Theme for StatusTheme {
    fn input_style(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Cancel => console::Style::new().dim().strikethrough(),
            cliclack::ThemeState::Submit => console::Style::new(),
            _ => console::Style::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn run(config: &Config, env: Option<&str>, all: bool) -> Result<()> {
    run_with_runner(config, env, all, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    env: Option<&str>,
    all: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let dashboard = Dashboard::build(config, env, runner)?;
    dashboard.render(all)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dashboard data model
// ---------------------------------------------------------------------------

struct SyncEntry {
    key: String,
    env: String,
    target: String,
    error: Option<String>,
    last_synced_at: Option<String>,
}

struct CoverageGap {
    key: String,
    missing_envs: Vec<String>,
    present_envs: Vec<String>,
}

struct Orphan {
    key: String,
    env: String,
}

#[derive(Clone)]
enum PluginStatus {
    Current,
    Stale { pushed: u64, local: u64 },
    Failed { version: u64, error: String },
    NeverPushed,
}

struct PluginState {
    name: String,
    env: String,
    status: PluginStatus,
}

struct NextStep {
    command: String,
    description: String,
}

struct Dashboard {
    project: String,
    version: u64,
    adapter_health: Vec<AdapterHealth>,
    #[allow(dead_code)]
    plugin_health: Vec<PluginHealth>,
    failed: Vec<SyncEntry>,
    pending: Vec<SyncEntry>,
    synced: Vec<SyncEntry>,
    unset: Vec<SyncEntry>,
    coverage_gaps: Vec<CoverageGap>,
    orphans: Vec<Orphan>,
    plugin_states: Vec<PluginState>,
    next_steps: Vec<NextStep>,
}

impl Dashboard {
    fn build(config: &Config, env: Option<&str>, runner: &dyn CommandRunner) -> Result<Self> {
        let store = SecretStore::open(&config.root)?;
        let payload = store.payload()?;
        let all_secrets = &payload.secrets;

        let index_path = config.root.join(".lockbox/sync-index.json");
        let index = SyncIndex::load(&index_path);
        let resolved = config.resolve_secrets()?;
        let adapter_names: Vec<&str> = config.adapter_names();

        let envs: Vec<&str> = match env {
            Some(e) => vec![e],
            None => config.environments.iter().map(|s| s.as_str()).collect(),
        };

        // 1. Health checks
        let adapter_health = check_adapter_health(config, runner);
        let plugin_health = check_plugin_health(config, runner);

        // 2. Sync entries
        let mut failed = Vec::new();
        let mut pending = Vec::new();
        let mut synced = Vec::new();
        let mut unset = Vec::new();

        for secret in &resolved {
            for target in &secret.targets {
                if !envs.contains(&target.environment.as_str()) {
                    continue;
                }
                if !adapter_names.contains(&target.adapter.as_str()) {
                    continue;
                }

                let composite = format!("{}:{}", secret.key, target.environment);
                let value = all_secrets.get(&composite);
                let tracker_key = SyncIndex::tracker_key(
                    &secret.key,
                    &target.adapter,
                    target.app.as_deref(),
                    &target.environment,
                );

                let record = index.records.get(&tracker_key);

                let entry = SyncEntry {
                    key: secret.key.clone(),
                    env: target.environment.clone(),
                    target: format_target(target),
                    error: record.and_then(|r| r.last_error.clone()),
                    last_synced_at: record.map(|r| r.last_synced_at.clone()),
                };

                match (value, record) {
                    (None, _) => unset.push(entry),
                    (Some(_), None) => pending.push(entry),
                    (Some(v), Some(rec)) => {
                        let current_hash = SyncIndex::hash_value(v);
                        if rec.last_sync_status == SyncStatus::Failed {
                            failed.push(SyncEntry {
                                error: Some(
                                    rec.last_error
                                        .as_deref()
                                        .unwrap_or("unknown error")
                                        .to_string(),
                                ),
                                ..entry
                            });
                        } else if current_hash != rec.value_hash {
                            pending.push(SyncEntry {
                                last_synced_at: Some(rec.last_synced_at.clone()),
                                ..entry
                            });
                        } else {
                            synced.push(entry);
                        }
                    }
                }
            }
        }

        // 3. Coverage gaps: secrets declared in config but missing values in some envs
        let mut coverage_gaps = Vec::new();
        for secret in &resolved {
            let secret_envs: BTreeSet<&str> = secret
                .targets
                .iter()
                .map(|t| t.environment.as_str())
                .collect();

            let mut missing_envs = Vec::new();
            let mut present_envs = Vec::new();

            for &e in &secret_envs {
                if !envs.contains(&e) {
                    continue;
                }
                let composite = format!("{}:{}", secret.key, e);
                if all_secrets.contains_key(&composite) {
                    present_envs.push(e.to_string());
                } else {
                    missing_envs.push(e.to_string());
                }
            }

            if !missing_envs.is_empty() && !present_envs.is_empty() {
                coverage_gaps.push(CoverageGap {
                    key: secret.key.clone(),
                    missing_envs,
                    present_envs,
                });
            }
        }

        // 4. Orphans: secrets in store but not in config
        let config_keys: BTreeSet<&str> = config
            .secrets
            .values()
            .flat_map(|vs| vs.keys().map(|k| k.as_str()))
            .collect();

        let mut orphans = Vec::new();
        for composite_key in all_secrets.keys() {
            if let Some((key, e)) = composite_key.rsplit_once(':') {
                if !envs.contains(&e) {
                    continue;
                }
                if !config_keys.contains(key) {
                    orphans.push(Orphan {
                        key: key.to_string(),
                        env: e.to_string(),
                    });
                }
            }
        }

        // 5. Plugin states
        let plugin_index_path = config.root.join(".lockbox/plugin-index.json");
        let plugin_index = PluginIndex::load(&plugin_index_path);
        let plugin_names: Vec<&String> = config.plugins.keys().collect();

        let mut plugin_states = Vec::new();
        for plugin_name in &plugin_names {
            for &env_name in &envs {
                let key = PluginIndex::tracker_key(plugin_name, env_name);
                let status = match plugin_index.records.get(&key) {
                    Some(record) if record.last_push_status == PushStatus::Failed => {
                        PluginStatus::Failed {
                            version: record.pushed_version,
                            error: record
                                .last_error
                                .as_deref()
                                .unwrap_or("unknown error")
                                .to_string(),
                        }
                    }
                    Some(record) if record.pushed_version >= payload.version => {
                        PluginStatus::Current
                    }
                    Some(record) => PluginStatus::Stale {
                        pushed: record.pushed_version,
                        local: payload.version,
                    },
                    None => PluginStatus::NeverPushed,
                };
                plugin_states.push(PluginState {
                    name: plugin_name.to_string(),
                    env: env_name.to_string(),
                    status,
                });
            }
        }

        // 6. Next steps
        let mut next_steps = Vec::new();

        // Failed syncs
        for entry in &failed {
            next_steps.push(NextStep {
                command: format!("lockbox sync --env {}", entry.env),
                description: format!("retry failed sync for {}:{}", entry.key, entry.env),
            });
        }

        // Pending syncs (dedupe by env)
        let mut pending_envs: BTreeSet<&str> = BTreeSet::new();
        for entry in &pending {
            pending_envs.insert(&entry.env);
        }
        for env_name in &pending_envs {
            let count = pending.iter().filter(|e| e.env == **env_name).count();
            next_steps.push(NextStep {
                command: format!("lockbox sync --env {env_name}"),
                description: format!(
                    "deploy {count} pending change{}",
                    if count == 1 { "" } else { "s" }
                ),
            });
        }

        // Coverage gaps
        for gap in &coverage_gaps {
            for missing_env in &gap.missing_envs {
                next_steps.push(NextStep {
                    command: format!("lockbox set {} --env {}", gap.key, missing_env),
                    description: "fill coverage gap".to_string(),
                });
            }
        }

        // Stale plugins
        for ps in &plugin_states {
            if let PluginStatus::Stale { pushed, local } = &ps.status {
                next_steps.push(NextStep {
                    command: format!("lockbox push --env {}", ps.env),
                    description: format!(
                        "plugin is {} version{} behind",
                        local - pushed,
                        if local - pushed == 1 { "" } else { "s" }
                    ),
                });
            }
            if let PluginStatus::NeverPushed = &ps.status {
                next_steps.push(NextStep {
                    command: format!("lockbox push --env {}", ps.env),
                    description: "plugin never pushed".to_string(),
                });
            }
        }

        // Orphans
        for orphan in &orphans {
            next_steps.push(NextStep {
                command: format!("lockbox delete {} --env {}", orphan.key, orphan.env),
                description: "remove orphaned secret".to_string(),
            });
        }

        // Deduplicate next steps by command
        let mut seen = BTreeSet::new();
        next_steps.retain(|s| seen.insert(s.command.clone()));

        Ok(Dashboard {
            project: config.project.clone(),
            version: payload.version,
            adapter_health,
            plugin_health,
            failed,
            pending,
            synced,
            unset,
            coverage_gaps,
            orphans,
            plugin_states,
            next_steps,
        })
    }

    fn render(&self, all: bool) -> Result<()> {
        cliclack::set_theme(StatusTheme);

        // Summary line
        let total = self.failed.len() + self.pending.len() + self.synced.len() + self.unset.len();
        let summary = if total == 0 {
            format!(
                "{} · {}",
                style(&self.project).bold(),
                style(format!("v{}", self.version)).dim(),
            )
        } else {
            let mut parts = Vec::new();
            if !self.synced.is_empty() {
                parts.push(format!("{} synced", self.synced.len()));
            }
            if !self.pending.is_empty() {
                parts.push(format!("{} pending", self.pending.len()));
            }
            if !self.failed.is_empty() {
                parts.push(format!("{} failed", self.failed.len()));
            }
            if !self.unset.is_empty() {
                parts.push(format!("{} unset", self.unset.len()));
            }

            let all_synced =
                self.failed.is_empty() && self.pending.is_empty() && self.unset.is_empty();

            if all_synced {
                format!(
                    "{} · {} · {}, all synced",
                    style(&self.project).bold(),
                    style(format!("v{}", self.version)).dim(),
                    style(format!(
                        "{total} target{}",
                        if total == 1 { "" } else { "s" }
                    )),
                )
            } else {
                format!(
                    "{} · {} · {} target{} ({})",
                    style(&self.project).bold(),
                    style(format!("v{}", self.version)).dim(),
                    total,
                    if total == 1 { "" } else { "s" },
                    parts.join(", "),
                )
            }
        };

        cliclack::intro(style(summary).to_string())?;

        // Targets section
        if !self.adapter_health.is_empty() {
            let lines: Vec<String> = self
                .adapter_health
                .iter()
                .map(|h| {
                    if h.ok {
                        format!(
                            "  {} {:<14} {}",
                            style("✓").green(),
                            h.name,
                            style(&h.message).dim()
                        )
                    } else {
                        format!(
                            "  {} {:<14} {}",
                            style("✗").red(),
                            h.name,
                            style(&h.message).dim()
                        )
                    }
                })
                .collect();
            cliclack::log::step(format!("Targets\n{}", lines.join("\n")))?;
        }

        // Sync section
        let has_problems =
            !self.failed.is_empty() || !self.pending.is_empty() || !self.unset.is_empty();

        if has_problems || (all && !self.synced.is_empty()) {
            let mut sync_lines = Vec::new();

            if !self.failed.is_empty() {
                sync_lines.push(format!(
                    "  {} {}",
                    style("✗").red(),
                    style(format!("{} failed", self.failed.len())).red().bold()
                ));
                for entry in &self.failed {
                    let freshness = entry
                        .last_synced_at
                        .as_deref()
                        .map(relative_time)
                        .unwrap_or_default();
                    let err_text = entry
                        .error
                        .as_deref()
                        .map(|e| format!(" {}", style(format!("({e})")).dim()))
                        .unwrap_or_default();
                    sync_lines.push(format!(
                        "     {}  → {}  {}{}",
                        style(format!("{}:{}", entry.key, entry.env)).dim(),
                        entry.target,
                        style(freshness).dim(),
                        err_text,
                    ));
                }
            }

            if !self.pending.is_empty() {
                sync_lines.push(format!(
                    "  {} {}",
                    style("●").yellow(),
                    style(format!("{} pending", self.pending.len()))
                        .yellow()
                        .bold()
                ));
                for entry in &self.pending {
                    let freshness = match &entry.last_synced_at {
                        Some(t) => {
                            let ago = relative_time(t);
                            if ago.is_empty() {
                                "never synced".to_string()
                            } else {
                                format!("last synced {ago}")
                            }
                        }
                        None => "never synced".to_string(),
                    };
                    sync_lines.push(format!(
                        "     {}  → {}  {}",
                        style(format!("{}:{}", entry.key, entry.env)).dim(),
                        entry.target,
                        style(freshness).dim(),
                    ));
                }
            }

            if !self.unset.is_empty() {
                sync_lines.push(format!(
                    "  {} {}",
                    style("○").dim(),
                    style(format!("{} unset", self.unset.len())).dim().bold()
                ));
                for entry in &self.unset {
                    sync_lines.push(format!(
                        "     {}  → {}",
                        style(format!("{}:{}", entry.key, entry.env)).dim(),
                        style(&entry.target).dim(),
                    ));
                }
            }

            if !self.synced.is_empty() {
                if all {
                    sync_lines.push(format!(
                        "  {} {}",
                        style("✓").green(),
                        style(format!("{} synced", self.synced.len()))
                            .green()
                            .bold()
                    ));
                    for entry in &self.synced {
                        let freshness = entry
                            .last_synced_at
                            .as_deref()
                            .map(relative_time)
                            .unwrap_or_default();
                        sync_lines.push(format!(
                            "     {}  → {}  {}",
                            style(format!("{}:{}", entry.key, entry.env)).dim(),
                            entry.target,
                            style(freshness).dim(),
                        ));
                    }
                } else {
                    sync_lines.push(format!(
                        "  {} {}  {}",
                        style("✓").green(),
                        style(format!("{} synced", self.synced.len())).green(),
                        style("(--all to show)").dim()
                    ));
                }
            }

            if !sync_lines.is_empty() {
                cliclack::log::step(format!("Sync\n{}", sync_lines.join("\n")))?;
            }
        }

        // Coverage section
        let has_coverage = !self.coverage_gaps.is_empty() || !self.orphans.is_empty();
        if has_coverage {
            let mut cov_lines = Vec::new();

            if !self.coverage_gaps.is_empty() {
                cov_lines.push(format!(
                    "  {} {} declared but never set",
                    style("○").dim(),
                    self.coverage_gaps.len()
                ));
                for gap in &self.coverage_gaps {
                    let present = gap.present_envs.join(", ");
                    for missing_env in &gap.missing_envs {
                        cov_lines.push(format!(
                            "     {}  missing in {} {}",
                            style(&gap.key).dim(),
                            style(missing_env).yellow(),
                            style(format!("(set in {present})")).dim(),
                        ));
                    }
                }
            }

            if !self.orphans.is_empty() {
                cov_lines.push(format!(
                    "  {} {} in store, not in config",
                    style("⚠").yellow(),
                    self.orphans.len()
                ));
                for orphan in &self.orphans {
                    cov_lines.push(format!(
                        "     {}",
                        style(format!("{}:{}", orphan.key, orphan.env)).dim(),
                    ));
                }
            }

            cliclack::log::step(format!("Coverage\n{}", cov_lines.join("\n")))?;
        }

        // Plugins section
        if !self.plugin_states.is_empty() {
            let lines: Vec<String> = self
                .plugin_states
                .iter()
                .map(|ps| match &ps.status {
                    PluginStatus::Current => format!(
                        "  {} {}  {}",
                        style("✓").green(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style(format!("v{}", self.version)).dim()
                    ),
                    PluginStatus::Stale { pushed, local } => format!(
                        "  {} {}  {}",
                        style("●").yellow(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style(format!("v{pushed} → local v{local}")).dim()
                    ),
                    PluginStatus::Failed { version, error } => format!(
                        "  {} {}  {} {}",
                        style("✗").red(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style(format!("v{version}")).dim(),
                        style(format!("({error})")).dim()
                    ),
                    PluginStatus::NeverPushed => format!(
                        "  {} {}  {}",
                        style("○").dim(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style("never pushed").dim()
                    ),
                })
                .collect();
            cliclack::log::step(format!("Plugins\n{}", lines.join("\n")))?;
        }

        // Next steps section
        if !self.next_steps.is_empty() {
            let cmd_width = self
                .next_steps
                .iter()
                .map(|s| s.command.len())
                .max()
                .unwrap_or(0);

            let lines: Vec<String> = self
                .next_steps
                .iter()
                .map(|s| {
                    format!(
                        "  {}  {}",
                        style(format!("{:<width$}", s.command, width = cmd_width)).cyan(),
                        style(&s.description).dim()
                    )
                })
                .collect();
            cliclack::log::step(format!("Next steps\n{}", lines.join("\n")))?;
        }

        cliclack::outro(
            style(format!("Store version: {}", self.version))
                .dim()
                .to_string(),
        )?;

        cliclack::reset_theme();

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn format_target(target: &ResolvedTarget) -> String {
    let mut s = target.adapter.clone();
    if let Some(app) = &target.app {
        s.push(':');
        s.push_str(app);
    }
    s
}

/// Convert an RFC3339 timestamp to a human-readable relative time string.
fn relative_time(timestamp: &str) -> String {
    let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) else {
        return String::new();
    };
    let delta = Utc::now().signed_duration_since(parsed);

    let days = delta.num_days();
    if days > 0 {
        return format!("{days}d ago");
    }
    let hours = delta.num_hours();
    if hours > 0 {
        return format!("{hours}h ago");
    }
    let minutes = delta.num_minutes();
    if minutes > 0 {
        return format!("{minutes}m ago");
    }
    "just now".to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_time_days() {
        let ts = (Utc::now() - chrono::Duration::days(3)).to_rfc3339();
        assert_eq!(relative_time(&ts), "3d ago");
    }

    #[test]
    fn relative_time_hours() {
        let ts = (Utc::now() - chrono::Duration::hours(5)).to_rfc3339();
        assert_eq!(relative_time(&ts), "5h ago");
    }

    #[test]
    fn relative_time_minutes() {
        let ts = (Utc::now() - chrono::Duration::minutes(12)).to_rfc3339();
        assert_eq!(relative_time(&ts), "12m ago");
    }

    #[test]
    fn relative_time_just_now() {
        let ts = Utc::now().to_rfc3339();
        assert_eq!(relative_time(&ts), "just now");
    }

    #[test]
    fn relative_time_invalid() {
        assert_eq!(relative_time("not-a-timestamp"), "");
    }
}
