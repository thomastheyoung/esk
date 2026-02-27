use std::fmt;

use chrono::{DateTime, Utc};
use console::{style, StyledObject};

// ---------------------------------------------------------------------------
// Icon vocabulary
// ---------------------------------------------------------------------------

pub fn icon_success() -> StyledObject<&'static str> {
    style("✔").green()
}
pub fn icon_failure() -> StyledObject<&'static str> {
    style("✗").red()
}
pub fn icon_pending() -> StyledObject<&'static str> {
    style("●").yellow()
}
pub fn icon_unset() -> StyledObject<&'static str> {
    style("○").dim()
}
pub fn icon_pruned() -> StyledObject<&'static str> {
    style("✂").yellow()
}
pub fn icon_warning() -> StyledObject<&'static str> {
    style("⚠").yellow()
}
pub fn icon_alert_yellow() -> StyledObject<&'static str> {
    style("!").yellow()
}
pub fn icon_alert_red() -> StyledObject<&'static str> {
    style("!").red()
}
pub fn icon_merge() -> StyledObject<&'static str> {
    style("↻").yellow()
}

// ---------------------------------------------------------------------------
// Text measurement
// ---------------------------------------------------------------------------

/// Returns the visible character count of a string, stripping ANSI escapes.
pub fn visible_width(text: &str) -> usize {
    console::strip_ansi_codes(text).chars().count()
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Formats a store version and optional timestamp into a compact label like `v3 (2m ago)`.
pub fn format_version_label(version: u64, timestamp: Option<&str>) -> String {
    match timestamp {
        Some(ts) => format!("v{} ({})", version, format_relative_time(ts)),
        None => format!("v{version}"),
    }
}

/// Builds a comma-separated count summary, skipping zero-count entries.
///
/// ```text
/// format_count_summary(&[("failed", 2), ("deployed", 5), ("unset", 0)])
/// // => "2 failed, 5 deployed"
/// ```
pub fn format_count_summary(counts: &[(&str, usize)]) -> String {
    counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(label, n)| format!("{n} {label}"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Section rendering (status dashboard)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub enum SectionColor {
    Red,
    Yellow,
    Green,
    Dim,
}

/// Renders a section header like `"  ✗ 3 failed"`.
pub fn section_header(icon: impl fmt::Display, label: &str, color: SectionColor) -> String {
    let styled = match color {
        SectionColor::Red => style(label).red().bold(),
        SectionColor::Yellow => style(label).yellow().bold(),
        SectionColor::Green => style(label).green().bold(),
        SectionColor::Dim => style(label).dim().bold(),
    };
    format!("  {icon} {styled}")
}

/// Renders a section entry like `"     API_KEY:prod  details here"`.
pub fn section_entry(left: &str, right: &str) -> String {
    format!("     {}  {}", style(left).dim(), right)
}

/// Like [`section_entry`] but pads `left` to `width` visible characters for column alignment.
pub fn section_entry_aligned(left: &str, right: &str, width: usize) -> String {
    let pad = width.saturating_sub(left.len());
    format!("     {}{}  {}", style(left).dim(), " ".repeat(pad), right)
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

/// Shared theme for all esk commands to ensure visual consistency.
pub struct EskTheme;

impl cliclack::Theme for EskTheme {
    /// Overridden to prevent cliclack from dimming the entire block body.
    /// This allows us to use per-fragment styling (green, red, dim) reliably.
    fn input_style(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Cancel => console::Style::new().dim().strikethrough(),
            _ => console::Style::new(),
        }
    }

    fn format_log(&self, text: &str, symbol: &str) -> String {
        self.format_log_with_spacing(text, symbol, true)
    }
}

/// Formats an RFC3339 timestamp into a human-friendly relative time.
pub fn format_relative_time(ts: &str) -> String {
    let Ok(dt) = DateTime::parse_from_rfc3339(ts) else {
        return ts.to_string();
    };
    let delta = Utc::now().signed_duration_since(dt.with_timezone(&Utc));

    if delta.num_seconds() < 60 {
        "just now".to_string()
    } else if delta.num_minutes() < 60 {
        format!("{}m ago", delta.num_minutes())
    } else if delta.num_hours() < 24 {
        format!("{}h ago", delta.num_hours())
    } else if delta.num_days() < 30 {
        format!("{}d ago", delta.num_days())
    } else {
        dt.format("%Y-%m-%d %H:%M").to_string()
    }
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

/// Default number of grouped entries to show before truncating.
pub const TRUNCATE_LIMIT: usize = 5;

/// Returns a footer like `"     ...and 12 more (--all to show)"` when `total > shown`,
/// or `None` when everything fits.
pub fn truncation_footer(total: usize, shown: usize) -> Option<String> {
    if total <= shown {
        return None;
    }
    let remaining = total - shown;
    Some(format!(
        "     {}",
        style(format!("...and {remaining} more (--all to show)")).dim()
    ))
}

// ---------------------------------------------------------------------------
// Dashboard alignment
// ---------------------------------------------------------------------------

/// Aligns a label and value with dots (...) between them for a dashboard look.
/// The `width` parameter is the total line width (label + dots + value).
pub fn format_dashboard_line(label: &str, value: &str, width: usize) -> String {
    let label_len = visible_width(label);
    let value_len = visible_width(value);

    if label_len + value_len + 2 >= width {
        return format!("{label}  {value}");
    }

    let dots = ".".repeat(width - label_len - value_len - 2);
    format!("{} {} {}", label, style(dots).dim(), value)
}

/// Aligns a label and value with dots, where `label_col` is the fixed column
/// where values start. All values align to the same X position regardless of
/// their length.
pub fn format_aligned_line(label: &str, value: &str, label_col: usize) -> String {
    let label_len = visible_width(label);

    if label_len + 2 >= label_col {
        return format!("{label}  {value}");
    }

    let dots = ".".repeat(label_col - label_len - 2);
    format!("{} {} {}", label, style(dots).dim(), value)
}
