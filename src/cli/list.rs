use anyhow::Result;
use console::style;
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::store::SecretStore;
use crate::tracker::{SyncIndex, SyncStatus};

/// Custom theme that renders note body text without dim styling.
///
/// The default cliclack theme wraps each note body line with `Style::new().dim()`.
/// When body lines contain their own ANSI styling (e.g. `style("✓").green()`),
/// the inner `\e[0m` reset breaks the outer dim — causing the first styled
/// fragment to inherit dim while subsequent ones don't. This produces
/// inconsistent colors (dim green vs bright green).
///
/// By overriding `input_style` to return an unstyled `Style`, we take full
/// control of per-fragment styling inside note bodies.
struct ListTheme;

impl cliclack::Theme for ListTheme {
    fn input_style(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Cancel => console::Style::new().dim().strikethrough(),
            cliclack::ThemeState::Submit => console::Style::new(),
            _ => console::Style::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum CellStatus {
    NotTargeted,
    Unset,
    Synced,
    Pending,
    Failed,
}

pub fn run(config: &Config, env: Option<&str>) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let all_secrets = store.list()?;

    if all_secrets.is_empty() {
        cliclack::log::info("No secrets stored. Run `lockbox set <KEY> --env <ENV>` to add one.")?;
        return Ok(());
    }

    cliclack::set_theme(ListTheme);

    let envs: Vec<&str> = match env {
        Some(e) => vec![e],
        None => config.environments.iter().map(|s| s.as_str()).collect(),
    };

    // Build sync status map: (key, env) → worst status across all targets
    let cell_statuses = build_cell_statuses(config, &all_secrets)?;

    // Build set of (key, env) pairs that have at least one configured target
    let resolved = config.resolve_secrets()?;
    let targeted: BTreeSet<(String, String)> = resolved
        .iter()
        .flat_map(|s| {
            s.targets
                .iter()
                .map(move |t| (s.key.clone(), t.environment.clone()))
        })
        .collect();

    let mut shown_keys: BTreeSet<String> = BTreeSet::new();

    // Collect uncategorized keys early so we can compute global key width
    let mut uncat_keys: BTreeSet<String> = BTreeSet::new();
    for composite_key in all_secrets.keys() {
        if let Some((key, _)) = composite_key.rsplit_once(':') {
            let in_config = config
                .secrets
                .values()
                .any(|vs| vs.contains_key(key));
            if !in_config {
                uncat_keys.insert(key.to_string());
            }
        }
    }

    // Compute global max key width across all groups for aligned columns
    let global_key_width = config
        .secrets
        .values()
        .flat_map(|vs| vs.keys().map(|k| k.len()))
        .chain(uncat_keys.iter().map(|k| k.len()))
        .max()
        .unwrap_or(0);

    for (vendor, vendor_secrets) in &config.secrets {
        let keys: Vec<&str> = vendor_secrets.keys().map(|k| k.as_str()).collect();
        if keys.is_empty() {
            continue;
        }
        for k in &keys {
            shown_keys.insert(k.to_string());
        }

        let body = render_table(&keys, &envs, global_key_width, |key, e| {
            let composite = format!("{key}:{e}");
            let has_value = all_secrets.contains_key(&composite);
            let is_targeted = targeted.contains(&(key.to_string(), e.to_string()));

            if !has_value && !is_targeted {
                CellStatus::NotTargeted
            } else if !has_value {
                CellStatus::Unset
            } else {
                cell_statuses
                    .get(&(key.to_string(), e.to_string()))
                    .copied()
                    .unwrap_or(CellStatus::Synced)
            }
        });

        cliclack::note(vendor, body)?;
    }

    if !uncat_keys.is_empty() {
        let keys: Vec<&str> = uncat_keys.iter().map(|s| s.as_str()).collect();

        let body = render_table(&keys, &envs, global_key_width, |key, e| {
            let composite = format!("{key}:{e}");
            if !all_secrets.contains_key(&composite) {
                CellStatus::NotTargeted
            } else {
                // Uncategorized keys have no configured targets
                CellStatus::Synced
            }
        });

        cliclack::note("Uncategorized (not in lockbox.yaml)", body)?;
    }

    cliclack::reset_theme();

    Ok(())
}

/// Compute the worst sync status for each (key, env) pair across all its targets.
fn build_cell_statuses(
    config: &Config,
    all_secrets: &BTreeMap<String, String>,
) -> Result<BTreeMap<(String, String), CellStatus>> {
    let resolved = config.resolve_secrets()?;
    let adapter_names: Vec<&str> = config.adapter_names();
    let index_path = config.root.join(".lockbox/sync-index.json");
    let index = SyncIndex::load(&index_path);

    let mut statuses: BTreeMap<(String, String), CellStatus> = BTreeMap::new();

    for secret in &resolved {
        for target in &secret.targets {
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

            let target_status = match (value, index.records.get(&tracker_key)) {
                (None, _) => continue, // no value — cell status determined by has_value check
                (Some(_), None) => CellStatus::Pending,
                (Some(v), Some(record)) => {
                    let current_hash = SyncIndex::hash_value(v);
                    if record.last_sync_status == SyncStatus::Failed {
                        CellStatus::Failed
                    } else if current_hash != record.value_hash {
                        CellStatus::Pending
                    } else {
                        CellStatus::Synced
                    }
                }
            };

            let cell_key = (secret.key.clone(), target.environment.clone());
            let current = statuses.entry(cell_key).or_insert(CellStatus::Synced);
            // Worst status wins (Failed > Pending > Synced)
            if target_status > *current {
                *current = target_status;
            }
        }
    }

    Ok(statuses)
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
        let mut row = style(format!("{:<width$}", key, width = key_width))
            .dim()
            .to_string();
        for (e, w) in envs.iter().zip(&col_widths) {
            let pad_left = *w / 2;
            let pad_right = *w - pad_left - 1;
            let indicator = match cell_status(key, e) {
                CellStatus::NotTargeted => " ".to_string(),
                CellStatus::Unset => style("○").dim().to_string(),
                CellStatus::Synced => style("✓").green().to_string(),
                CellStatus::Pending => style("●").yellow().to_string(),
                CellStatus::Failed => style("✗").red().to_string(),
            };
            row.push_str(&format!(
                "{}{}{}{}",
                " ".repeat(gap),
                " ".repeat(pad_left),
                indicator,
                " ".repeat(pad_right),
            ));
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
