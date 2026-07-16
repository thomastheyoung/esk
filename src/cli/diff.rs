use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use anyhow::Result;

use crate::config::Config;
use crate::store::SecretStore;

#[derive(Debug, Default, PartialEq, Eq)]
struct DiffReport {
    only_left: BTreeSet<String>,
    only_right: BTreeSet<String>,
    changed: BTreeMap<String, (String, String)>,
}

impl DiffReport {
    fn is_empty(&self) -> bool {
        self.only_left.is_empty() && self.only_right.is_empty() && self.changed.is_empty()
    }

    fn render(&self, left: &str, right: &str, show_values: bool) -> String {
        if self.is_empty() {
            return format!("No differences between {left} and {right}.\n");
        }

        let mut output = String::new();
        if !self.only_left.is_empty() {
            writeln!(output, "Only in {left}:").unwrap();
            for key in &self.only_left {
                writeln!(output, "  {key}").unwrap();
            }
        }
        if !self.only_right.is_empty() {
            writeln!(output, "Only in {right}:").unwrap();
            for key in &self.only_right {
                writeln!(output, "  {key}").unwrap();
            }
        }
        if !self.changed.is_empty() {
            writeln!(output, "Changed:").unwrap();
            for (key, (left_value, right_value)) in &self.changed {
                if show_values {
                    writeln!(output, "  {key}: {left_value:?} -> {right_value:?}").unwrap();
                } else {
                    writeln!(output, "  {key}").unwrap();
                }
            }
        }
        output
    }
}

fn env_secrets(secrets: &BTreeMap<String, String>, env: &str) -> BTreeMap<String, String> {
    let suffix = format!(":{env}");
    secrets
        .iter()
        .filter_map(|(composite, value)| {
            composite
                .strip_suffix(&suffix)
                .map(|key| (key.to_string(), value.clone()))
        })
        .collect()
}

fn build_report(left: &BTreeMap<String, String>, right: &BTreeMap<String, String>) -> DiffReport {
    let mut report = DiffReport::default();

    for key in left.keys() {
        match right.get(key) {
            Some(right_value) if left[key] != *right_value => {
                report
                    .changed
                    .insert(key.clone(), (left[key].clone(), right_value.clone()));
            }
            Some(_) => {}
            None => {
                report.only_left.insert(key.clone());
            }
        }
    }
    for key in right.keys() {
        if !left.contains_key(key) {
            report.only_right.insert(key.clone());
        }
    }

    report
}

pub fn run(config: &Config, left_env: &str, right_env: &str, show_values: bool) -> Result<()> {
    config.validate_env(left_env)?;
    config.validate_env(right_env)?;
    if left_env == right_env {
        println!("No differences between {left_env} and {right_env}.");
        return Ok(());
    }

    let payload = SecretStore::open(&config.root)?.payload()?;
    let left = env_secrets(&payload.secrets, left_env);
    let right = env_secrets(&payload.secrets, right_env);
    let report = build_report(&left, &right);
    print!("{}", report.render(left_env, right_env, show_values));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn report_separates_added_removed_and_changed_keys() {
        let report = build_report(
            &map(&[("A", "same"), ("B", "old"), ("LEFT", "value")]),
            &map(&[("A", "same"), ("B", "new"), ("RIGHT", "value")]),
        );

        assert_eq!(report.only_left, BTreeSet::from(["LEFT".to_string()]));
        assert_eq!(report.only_right, BTreeSet::from(["RIGHT".to_string()]));
        assert_eq!(
            report.changed,
            BTreeMap::from([("B".to_string(), ("old".to_string(), "new".to_string()))])
        );
    }

    #[test]
    fn default_render_does_not_include_values() {
        let report = build_report(&map(&[("KEY", "old")]), &map(&[("KEY", "new")]));
        let output = report.render("dev", "prod", false);

        assert!(output.contains("  KEY\n"));
        assert!(!output.contains("old"));
        assert!(!output.contains("new"));
    }

    #[test]
    fn values_render_only_when_requested_and_are_escaped() {
        let report = build_report(&map(&[("KEY", "old\nvalue")]), &map(&[("KEY", "new")]));
        let output = report.render("dev", "prod", true);

        assert!(output.contains(r#"KEY: "old\nvalue" -> "new""#));
    }
}
