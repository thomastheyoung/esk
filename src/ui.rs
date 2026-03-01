use std::fmt;

use chrono::{DateTime, Utc};
use console::style;

// ---------------------------------------------------------------------------
// Icon vocabulary
// ---------------------------------------------------------------------------

/// Semantic icon vocabulary. Each variant carries a default color via [`fmt::Display`],
/// but any icon can be recolored with [`Icon::color`] for composed combinations.
#[derive(Clone, Copy)]
pub enum Icon {
    Success, // ✔
    Failure, // ✗
    Pending, // ●
    Unset,   // ○
    Pruned,  // ✂
    Warning, // !
    Merge,   // ↻
}

impl Icon {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Success => "✔",
            Self::Failure => "✗",
            Self::Pending => "●",
            Self::Unset => "○",
            Self::Pruned => "✂",
            Self::Warning => "!",
            Self::Merge => "↻",
        }
    }

    fn default_color(self) -> SectionColor {
        match self {
            Self::Success => SectionColor::Green,
            Self::Failure => SectionColor::Red,
            Self::Pending | Self::Pruned | Self::Warning | Self::Merge => SectionColor::Yellow,
            Self::Unset => SectionColor::Dim,
        }
    }

    /// Render this icon in a specific color, overriding the default.
    pub fn color(self, color: SectionColor) -> String {
        apply_color(self.glyph(), color)
    }
}

impl fmt::Display for Icon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.color(self.default_color()))
    }
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

/// Builds a deploy summary like "deployed 6 keys to 7 targets" or
/// "deployed 6 keys to 7 targets, 2 failed".
pub fn format_deploy_summary(
    keys: usize,
    deployed: usize,
    failed: usize,
    unset: usize,
    pruned: usize,
) -> String {
    let keys_str = style(format!("{keys} keys")).bold().to_string();
    let targets_str = style(format!("{deployed} targets")).bold().to_string();
    let mut parts = vec![format!("deployed {keys_str} to {targets_str}")];
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if unset > 0 {
        parts.push(format!("{unset} unset"));
    }
    if pruned > 0 {
        parts.push(format!("{pruned} pruned"));
    }
    parts.join(", ")
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

/// Apply a [`SectionColor`] to a string, returning the styled result.
fn apply_color(text: &str, color: SectionColor) -> String {
    match color {
        SectionColor::Red => style(text).red().to_string(),
        SectionColor::Yellow => style(text).yellow().to_string(),
        SectionColor::Green => style(text).green().to_string(),
        SectionColor::Dim => style(text).dim().to_string(),
    }
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
// Spinner animation
// ---------------------------------------------------------------------------

pub const SPINNER_FRAMES: &[char] = &[
    '\u{280B}', '\u{2819}', '\u{2839}', '\u{2838}', '\u{283C}', '\u{2834}', '\u{2826}', '\u{2827}',
    '\u{2807}', '\u{280F}',
];
pub const SPINNER_INTERVAL: std::time::Duration = std::time::Duration::from_millis(80);

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
