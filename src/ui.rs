use chrono::{DateTime, Utc};
use console::style;

/// Shared theme for all esk commands to ensure visual consistency.
pub struct EskTheme;

impl cliclack::Theme for EskTheme {
    /// Overridden to prevent cliclack from dimming the entire block body.
    /// This allows us to use per-fragment styling (green, red, dim) reliably.
    fn input_style(&self, state: &cliclack::ThemeState) -> console::Style {
        match state {
            cliclack::ThemeState::Cancel => console::Style::new().dim().strikethrough(),
            cliclack::ThemeState::Submit => console::Style::new(),
            _ => console::Style::new(),
        }
    }

    fn format_log(&self, text: &str, symbol: &str) -> String {
        // Keep compact one-line log rows while preserving cliclack's newline
        // handling, so sequential logs don't get concatenated.
        self.format_log_with_spacing(text, symbol, false)
    }
}

/// Formats an RFC3339 timestamp into a human-friendly relative time.
pub fn format_relative_time(ts: &str) -> String {
    if let Ok(dt) = DateTime::parse_from_rfc3339(ts) {
        let now = Utc::now();
        let duration = now.signed_duration_since(dt.with_timezone(&Utc));

        if duration.num_seconds() < 60 {
            "just now".to_string()
        } else if duration.num_minutes() < 60 {
            format!("{}m ago", duration.num_minutes())
        } else if duration.num_hours() < 24 {
            format!("{}h ago", duration.num_hours())
        } else {
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
    } else {
        ts.to_string()
    }
}

/// Aligns a label and value with dots (...) between them for a dashboard look.
pub fn format_dashboard_line(label: &str, value: &str, width: usize) -> String {
    let label_len = console::strip_ansi_codes(label).chars().count();
    let value_len = console::strip_ansi_codes(value).chars().count();

    if label_len + value_len + 2 >= width {
        return format!("{}  {}", label, value);
    }

    let dots = ".".repeat(width - label_len - value_len - 2);
    format!("{} {} {}", label, style(dots).dim(), value)
}
