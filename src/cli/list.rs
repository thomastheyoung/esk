use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::Result;
use console::style;

use crate::config::{Config, ResolvedSecret};
use crate::deploy_tracker::{DeployIndex, DeployStatus};
use crate::store::SecretStore;
use crate::ui;

struct ListGroup {
    name: String,
    table: String,
}

struct ListReport {
    groups: Vec<ListGroup>,
}

impl ListReport {
    fn render(&self) -> Result<()> {
        if self.groups.is_empty() {
            cliclack::log::info("No secrets stored. Run `esk set <KEY> --env <ENV>` to add one.")?;
            return Ok(());
        }

        for group in &self.groups {
            cliclack::note(&group.name, &group.table)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CellStatus {
    NotTargeted,
    Unset,
    Deployed,
    Pending,
    Failed,
}

pub fn run(config: &Config, env: Option<&str>) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let all_secrets = store.list()?;
    let payload = store.payload()?;
    let version = payload.version;

    // Count unique keys (composite keys are "KEY:env")
    let unique_keys: BTreeSet<&str> = all_secrets
        .keys()
        .filter_map(|k| k.rsplit_once(':').map(|(key, _)| key))
        .collect();

    let scope = match env {
        Some(e) => e.to_string(),
        None => format!(
            "{} env{}",
            config.environments.len(),
            if config.environments.len() == 1 {
                ""
            } else {
                "s"
            }
        ),
    };

    cliclack::intro(
        style(format!(
            "{} · {} · {} secret{} · {}",
            style(&config.project).bold(),
            style(format!("v{version}")).dim(),
            unique_keys.len(),
            if unique_keys.len() == 1 { "" } else { "s" },
            scope,
        ))
        .to_string(),
    )?;

    if all_secrets.is_empty() {
        let report = ListReport { groups: Vec::new() };
        return report.render();
    }

    let envs: Vec<&str> = match env {
        Some(e) => vec![e],
        None => config
            .environments
            .iter()
            .map(std::string::String::as_str)
            .collect(),
    };

    let resolved = config.resolve_secrets()?;

    // Build deploy status map: (key, env) → worst status across all targets
    let cell_statuses = build_cell_statuses(config, &resolved, &all_secrets);

    // Build set of (key, env) pairs that have at least one configured target
    let targeted: BTreeSet<(&str, &str)> = resolved
        .iter()
        .flat_map(|s| {
            s.targets
                .iter()
                .map(move |t| (s.key.as_str(), t.environment.as_str()))
        })
        .collect();

    // Collect uncategorized keys early so we can compute global key width
    let mut uncat_keys: BTreeSet<String> = BTreeSet::new();
    for composite_key in all_secrets.keys() {
        if let Some((key, _)) = composite_key.rsplit_once(':') {
            let in_config = config.secrets.values().any(|vs| vs.contains_key(key));
            if !in_config {
                uncat_keys.insert(key.to_string());
            }
        }
    }

    // Compute global max key width across all groups for aligned columns
    let global_key_width = config
        .secrets
        .values()
        .flat_map(|vs| vs.keys().map(std::string::String::len))
        .chain(uncat_keys.iter().map(std::string::String::len))
        .max()
        .unwrap_or(0);

    let mut groups: Vec<ListGroup> = Vec::new();

    for (group, group_secrets) in &config.secrets {
        let keys: Vec<&str> = group_secrets
            .keys()
            .map(std::string::String::as_str)
            .collect();
        if keys.is_empty() {
            continue;
        }

        let body = render_table(&keys, &envs, global_key_width, |key, e| {
            let composite = format!("{key}:{e}");
            let has_value = all_secrets.contains_key(&composite);
            let is_targeted = targeted.contains(&(key, e));

            if !has_value && !is_targeted {
                CellStatus::NotTargeted
            } else if !has_value {
                CellStatus::Unset
            } else {
                cell_statuses
                    .get(&(key.to_string(), e.to_string()))
                    .copied()
                    .unwrap_or(CellStatus::Deployed)
            }
        });

        groups.push(ListGroup {
            name: group.clone(),
            table: body,
        });
    }

    if !uncat_keys.is_empty() {
        let keys: Vec<&str> = uncat_keys.iter().map(std::string::String::as_str).collect();

        let body = render_table(&keys, &envs, global_key_width, |key, e| {
            let composite = format!("{key}:{e}");
            if all_secrets.contains_key(&composite) {
                CellStatus::Deployed
            } else {
                CellStatus::NotTargeted
            }
        });

        groups.push(ListGroup {
            name: "Uncategorized (not in esk.yaml)".to_string(),
            table: body,
        });
    }

    let report = ListReport { groups };
    report.render()?;

    let env_versions: Vec<(String, u64)> = config
        .environments
        .iter()
        .map(|e| (e.clone(), payload.env_version(e)))
        .collect();
    cliclack::outro(
        style(ui::format_store_outro(version, &env_versions, env))
            .dim()
            .to_string(),
    )?;
    Ok(())
}

/// Compute the worst deploy status for each (key, env) pair across all its targets.
fn build_cell_statuses(
    config: &Config,
    resolved: &[ResolvedSecret],
    all_secrets: &BTreeMap<String, String>,
) -> BTreeMap<(String, String), CellStatus> {
    let target_names: Vec<&str> = config.target_names();
    let index_path = config.root.join(".esk/deploy-index.json");
    let index = DeployIndex::load(&index_path);

    let mut statuses: BTreeMap<(String, String), CellStatus> = BTreeMap::new();

    for secret in resolved {
        for target in &secret.targets {
            if !target_names.contains(&target.service.as_str()) {
                continue;
            }

            let composite = format!("{}:{}", secret.key, target.environment);
            let value = all_secrets.get(&composite);
            let tracker_key = DeployIndex::tracker_key(
                &secret.key,
                &target.service,
                target.app.as_deref(),
                &target.environment,
            );

            let target_status = match (value, index.records.get(&tracker_key)) {
                (None, _) => continue, // no value — cell status determined by has_value check
                (Some(_), None) => CellStatus::Pending,
                (Some(v), Some(record)) => {
                    let current_hash = DeployIndex::hash_value(v);
                    if record.last_deploy_status == DeployStatus::Failed {
                        CellStatus::Failed
                    } else if current_hash != record.value_hash {
                        CellStatus::Pending
                    } else {
                        CellStatus::Deployed
                    }
                }
            };

            let cell_key = (secret.key.clone(), target.environment.clone());
            let current = statuses.entry(cell_key).or_insert(CellStatus::Deployed);
            // Worst status wins (Failed > Pending > Deployed)
            if target_status > *current {
                *current = target_status;
            }
        }
    }

    statuses
}

fn render_table(
    keys: &[&str],
    envs: &[&str],
    key_width: usize,
    cell_status: impl Fn(&str, &str) -> CellStatus,
) -> String {
    let col_widths: Vec<usize> = envs.iter().map(|e| e.len().max(1)).collect();
    let gap = 2;

    // Header line
    let mut header = " ".repeat(key_width);
    for (e, w) in envs.iter().zip(&col_widths) {
        header.push_str(&" ".repeat(gap));
        header.push_str(&center(e, *w));
    }

    let mut lines = vec![style(header).dim().to_string()];

    // Data rows
    for key in keys {
        let mut row = style(format!("{key:<key_width$}")).dim().to_string();
        for (e, w) in envs.iter().zip(&col_widths) {
            let pad_left = *w / 2;
            let pad_right = *w - pad_left - 1;
            let indicator = match cell_status(key, e) {
                CellStatus::NotTargeted => " ".to_string(),
                CellStatus::Unset => ui::Icon::Unset.to_string(),
                CellStatus::Deployed => ui::Icon::Success.to_string(),
                CellStatus::Pending => ui::Icon::Pending.to_string(),
                CellStatus::Failed => ui::Icon::Failure.to_string(),
            };
            let _ = write!(
                row,
                "{}{}{}{}",
                " ".repeat(gap),
                " ".repeat(pad_left),
                indicator,
                " ".repeat(pad_right),
            );
        }
        lines.push(row);
    }

    lines.join("\n")
}

fn center(s: &str, width: usize) -> String {
    if s.len() >= width {
        return s.to_string();
    }
    let pad_left = (width - s.len()) / 2;
    let pad_right = width - s.len() - pad_left;
    format!("{}{}{}", " ".repeat(pad_left), s, " ".repeat(pad_right))
}
