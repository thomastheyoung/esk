use console::style;

/// Levenshtein edit distance between two strings (case-insensitive).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let m = a.len();
    let n = b.len();

    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for (i, row) in dp.iter_mut().enumerate().take(m + 1) {
        row[0] = i;
    }
    for (j, cell) in dp[0].iter_mut().enumerate().take(n + 1) {
        *cell = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = if a[i - 1] == b[j - 1] {
                dp[i - 1][j - 1]
            } else {
                1 + dp[i - 1][j - 1].min(dp[i - 1][j]).min(dp[i][j - 1])
            };
        }
    }
    dp[m][n]
}

fn join<S: AsRef<str>>(items: &[S]) -> String {
    items
        .iter()
        .map(|s| s.as_ref())
        .collect::<Vec<_>>()
        .join(", ")
}

fn hint_line(suggestion: &str) -> String {
    format!(
        "\n  {} did you mean {}?",
        style("hint:").yellow(),
        style(suggestion).green().bold()
    )
}

fn valid_line(label: &str, items: &str) -> String {
    format!("\n  {} {}", style(format!("{label}:")).dim(), items)
}

/// Returns the closest candidate to `input`, if within a reasonable edit distance.
pub fn closest<'a, S: AsRef<str>>(input: &str, candidates: &'a [S]) -> Option<&'a str> {
    // Allow up to 1/3 of the input length in edits, minimum 1, maximum 3.
    let threshold = (input.len() / 3 + 1).clamp(1, 3);
    candidates
        .iter()
        .filter_map(|c| {
            let s = c.as_ref();
            let d = levenshtein(input, s);
            if d <= threshold {
                Some((d, s))
            } else {
                None
            }
        })
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Builds an "unknown environment" error message with an optional typo suggestion.
pub fn unknown_env<S: AsRef<str>>(env: &str, environments: &[S]) -> String {
    let valid = join(environments);
    let mut msg = format!("unknown environment '{}'", style(env).bold());
    if let Some(suggestion) = closest(env, environments) {
        msg.push_str(&hint_line(suggestion));
    }
    msg.push_str(&valid_line("valid", &valid));
    msg
}

/// Builds an "unknown environment in target" error message (for config validation).
pub fn unknown_env_in_target<S: AsRef<str>>(env: &str, target: &str, environments: &[S]) -> String {
    let valid = join(environments);
    let mut msg = format!(
        "unknown environment '{}' in target '{}'",
        style(env).bold(),
        target
    );
    if let Some(suggestion) = closest(env, environments) {
        msg.push_str(&hint_line(suggestion));
    }
    msg.push_str(&valid_line("valid", &valid));
    msg
}

/// Builds an "unknown remote" error message (for --only flag validation).
pub fn unknown_remote<S: AsRef<str>>(name: &str, remote_names: &[S]) -> String {
    let available = join(remote_names);
    let mut msg = format!("unknown remote '{}'", style(name).bold());
    if let Some(suggestion) = closest(name, remote_names) {
        msg.push_str(&hint_line(suggestion));
    }
    msg.push_str(&valid_line("available", &available));
    msg
}

/// Builds an "adapter not configured" error message (for config validation).
pub fn unknown_target<S: AsRef<str>>(adapter: &str, target_names: &[S]) -> String {
    let configured = join(target_names);
    let mut msg = format!("target '{}' is not configured", style(adapter).bold());
    if let Some(suggestion) = closest(adapter, target_names) {
        msg.push_str(&hint_line(suggestion));
    }
    msg.push_str(&valid_line("configured", &configured));
    msg
}

/// Builds an "unknown app in target" error message (for config validation).
pub fn unknown_app_in_target<S: AsRef<str>>(app: &str, target: &str, app_names: &[S]) -> String {
    let configured = join(app_names);
    let mut msg = format!("unknown app '{}' in target '{}'", style(app).bold(), target);
    if let Some(suggestion) = closest(app, app_names) {
        msg.push_str(&hint_line(suggestion));
    }
    msg.push_str(&valid_line("configured", &configured));
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn strip(s: &str) -> String {
        console::strip_ansi_codes(s).to_string()
    }

    #[test]
    fn exact_match_returns_itself() {
        assert_eq!(closest("dev", &envs(&["dev", "prod"])), Some("dev"));
    }

    #[test]
    fn single_typo_suggests_closest() {
        assert_eq!(closest("dv", &envs(&["dev", "prod"])), Some("dev"));
        assert_eq!(closest("prdo", &envs(&["dev", "prod"])), Some("prod"));
        assert_eq!(
            closest("staging", &envs(&["dev", "staging", "prod"])),
            Some("staging")
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(closest("DEV", &envs(&["dev", "prod"])), Some("dev"));
        assert_eq!(closest("Prod", &envs(&["dev", "prod"])), Some("prod"));
    }

    #[test]
    fn no_suggestion_for_completely_different() {
        assert_eq!(closest("xyz", &envs(&["dev", "prod"])), None);
        assert_eq!(closest("test", &envs(&["dev", "prod"])), None);
    }

    #[test]
    fn works_with_str_slices() {
        let candidates: Vec<&str> = vec!["env", "cloudflare", "convex"];
        assert_eq!(closest("cloudflar", &candidates), Some("cloudflare"));
        assert_eq!(closest("envv", &candidates), Some("env"));
        assert_eq!(closest("xyz", &candidates), None);
    }

    #[test]
    fn empty_candidates_returns_none() {
        let empty: Vec<String> = vec![];
        assert_eq!(closest("dev", &empty), None);
    }

    // --- Message formatting tests ---

    #[test]
    fn unknown_env_includes_suggestion() {
        let msg = strip(&unknown_env("stging", &envs(&["dev", "staging", "prod"])));
        assert!(msg.contains("unknown environment 'stging'"));
        assert!(msg.contains("did you mean staging"));
        assert!(msg.contains("valid:"));
    }

    #[test]
    fn unknown_env_no_suggestion() {
        let msg = strip(&unknown_env("xyz", &envs(&["dev", "prod"])));
        assert!(msg.contains("unknown environment 'xyz'"));
        assert!(!msg.contains("did you mean"));
        assert!(msg.contains("valid:"));
    }

    #[test]
    fn unknown_env_in_target_with_suggestion() {
        let msg = strip(&unknown_env_in_target(
            "dv",
            "web:dv",
            &envs(&["dev", "prod"]),
        ));
        assert!(msg.contains("unknown environment 'dv' in target 'web:dv'"));
        assert!(msg.contains("did you mean dev"));
        assert!(msg.contains("valid:"));
    }

    #[test]
    fn unknown_env_in_target_no_suggestion() {
        let msg = strip(&unknown_env_in_target(
            "xyz",
            "web:xyz",
            &envs(&["dev", "prod"]),
        ));
        assert!(msg.contains("unknown environment 'xyz' in target 'web:xyz'"));
        assert!(!msg.contains("did you mean"));
    }

    #[test]
    fn unknown_remote_with_suggestion() {
        let msg = strip(&unknown_remote(
            "1pasword",
            &envs(&["1password", "s3", "vault"]),
        ));
        assert!(msg.contains("unknown remote '1pasword'"));
        assert!(msg.contains("did you mean 1password"));
        assert!(msg.contains("available:"));
    }

    #[test]
    fn unknown_remote_no_suggestion() {
        let msg = strip(&unknown_remote("xyz", &envs(&["1password", "s3"])));
        assert!(msg.contains("unknown remote 'xyz'"));
        assert!(!msg.contains("did you mean"));
        assert!(msg.contains("available:"));
    }

    #[test]
    fn unknown_target_with_suggestion() {
        let candidates: Vec<&str> = vec!["env", "cloudflare", "convex"];
        let msg = strip(&unknown_target("cloudflar", &candidates));
        assert!(msg.contains("target 'cloudflar' is not configured"));
        assert!(msg.contains("did you mean cloudflare"));
        assert!(msg.contains("configured:"));
    }

    #[test]
    fn unknown_target_no_suggestion() {
        let candidates: Vec<&str> = vec!["env"];
        let msg = strip(&unknown_target("xyz", &candidates));
        assert!(msg.contains("target 'xyz' is not configured"));
        assert!(!msg.contains("did you mean"));
        assert!(msg.contains("configured: env"));
    }

    #[test]
    fn unknown_app_in_target_with_suggestion() {
        let msg = strip(&unknown_app_in_target(
            "wbe",
            "wbe:dev",
            &envs(&["web", "api"]),
        ));
        assert!(msg.contains("unknown app 'wbe' in target 'wbe:dev'"));
        assert!(msg.contains("did you mean web"));
        assert!(msg.contains("configured:"));
    }

    #[test]
    fn unknown_app_in_target_no_suggestion() {
        let msg = strip(&unknown_app_in_target("xyz", "xyz:dev", &envs(&["web"])));
        assert!(msg.contains("unknown app 'xyz' in target 'xyz:dev'"));
        assert!(!msg.contains("did you mean"));
        assert!(msg.contains("configured: web"));
    }
}
