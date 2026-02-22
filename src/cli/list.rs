use anyhow::Result;
use console::style;

use crate::config::Config;
use crate::store::SecretStore;

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

pub fn run(config: &Config, env: Option<&str>) -> Result<()> {
    let store = SecretStore::open(&config.root)?;
    let all_secrets = store.list()?;

    if all_secrets.is_empty() {
        cliclack::log::info("No secrets stored. Run `lockbox set <KEY> --env <ENV>` to add one.")?;
        return Ok(());
    }

    // Use a theme that doesn't dim note body text, so our styled indicators
    // render with consistent colors.
    cliclack::set_theme(ListTheme);

    let envs: Vec<&str> = match env {
        Some(e) => vec![e],
        None => config.environments.iter().map(|s| s.as_str()).collect(),
    };

    let mut shown_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Collect uncategorized keys early so we can compute global key width
    let mut uncat_keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
            all_secrets.contains_key(&composite)
        });

        cliclack::note(vendor, body)?;
    }

    if !uncat_keys.is_empty() {
        let keys: Vec<&str> = uncat_keys.iter().map(|s| s.as_str()).collect();

        let body = render_table(&keys, &envs, global_key_width, |key, e| {
            let composite = format!("{key}:{e}");
            all_secrets.contains_key(&composite)
        });

        cliclack::note("Uncategorized (not in lockbox.yaml)", body)?;
    }

    cliclack::reset_theme();

    Ok(())
}

fn render_table(
    keys: &[&str],
    envs: &[&str],
    key_width: usize,
    has_value: impl Fn(&str, &str) -> bool,
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
            let indicator = if has_value(key, e) {
                style("✓").green().to_string()
            } else {
                style("·").dim().to_string()
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
