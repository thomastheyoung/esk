use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::adapters::{CommandOpts, CommandRunner, SyncAdapter};
use crate::config::{Config, OnePasswordAdapterConfig, ResolvedTarget};

pub struct OnePasswordAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a OnePasswordAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> SyncAdapter for OnePasswordAdapter<'a> {
    fn name(&self) -> &str {
        "onepassword"
    }

    fn sync_secret(&self, _key: &str, _value: &str, _target: &ResolvedTarget) -> Result<()> {
        // 1Password sync is done via push/pull, not per-secret sync
        Ok(())
    }
}

impl<'a> OnePasswordAdapter<'a> {
    fn item_name(&self, env: &str) -> Result<String> {
        self.config.onepassword_item_name(env)
    }

    /// Get a 1Password item, returning None if it doesn't exist.
    pub fn get_item(&self, env: &str) -> Result<Option<OpItem>> {
        let item_name = self.item_name(env)?;
        let vault = &self.adapter_config.vault;

        let output = self
            .runner
            .run(
                "op",
                &[
                    "item", "get", &item_name, "--vault", vault, "--format", "json",
                ],
                CommandOpts::default(),
            )
            .context("failed to run op CLI")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("isn't an item") || stderr.contains("not found") {
                return Ok(None);
            }
            anyhow::bail!("op item get failed: {stderr}");
        }

        let json: Value =
            serde_json::from_slice(&output.stdout).context("failed to parse op output")?;

        Ok(Some(OpItem::from_json(&json)?))
    }

    /// Push secrets to a 1Password item. Creates or updates.
    /// `secrets` should contain bare keys (not composite "KEY:env" keys).
    pub fn push_item(
        &self,
        env: &str,
        secrets: &BTreeMap<String, String>,
        version: u64,
    ) -> Result<()> {
        let item_name = self.item_name(env)?;
        let vault = &self.adapter_config.vault;

        let existing = self.get_item(env)?;

        // Build field assignments: "vendor.key[concealed]=value"
        let mut assignments: Vec<String> = Vec::new();

        // Group secrets by vendor using the config
        let mut by_vendor: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (key, value) in secrets {
            let vendor = self
                .config
                .find_secret(key)
                .map(|(v, _)| v)
                .unwrap_or_else(|| "General".to_string());
            by_vendor
                .entry(vendor)
                .or_default()
                .push((key.clone(), value.clone()));
        }

        for (vendor, entries) in &by_vendor {
            for (key, value) in entries {
                assignments.push(format!("{vendor}.{key}[concealed]={value}"));
            }
        }

        // Add version metadata
        assignments.push(format!("_Metadata.version[text]={version}"));

        if existing.is_some() {
            // Update existing item
            let mut args: Vec<String> = vec![
                "item".to_string(),
                "edit".to_string(),
                item_name,
                "--vault".to_string(),
                vault.clone(),
            ];
            for assignment in &assignments {
                args.push(assignment.clone());
            }
            let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let output = self
                .runner
                .run("op", &args_ref, CommandOpts::default())
                .context("failed to run op item edit")?;
            if !output.success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("op item edit failed: {stderr}");
            }
        } else {
            // Create new item
            let mut args: Vec<String> = vec![
                "item".to_string(),
                "create".to_string(),
                "--category".to_string(),
                "Secure Note".to_string(),
                "--title".to_string(),
                item_name,
                "--vault".to_string(),
                vault.clone(),
            ];
            for assignment in &assignments {
                args.push(assignment.clone());
            }
            let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
            let output = self
                .runner
                .run("op", &args_ref, CommandOpts::default())
                .context("failed to run op item create")?;
            if !output.success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("op item create failed: {stderr}");
            }
        }

        Ok(())
    }

    /// Pull secrets from a 1Password item.
    pub fn pull_item(&self, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let item = match self.get_item(env)? {
            Some(item) => item,
            None => return Ok(None),
        };
        Ok(Some((item.secrets, item.version)))
    }
}

#[derive(Debug)]
pub struct OpItem {
    pub secrets: BTreeMap<String, String>,
    pub version: u64,
}

impl OpItem {
    /// Parse a 1Password item from JSON.
    pub fn from_json(json: &Value) -> Result<Self> {
        let mut secrets = BTreeMap::new();
        let mut version = 0u64;

        let fields = json["fields"].as_array().context("op item has no fields")?;

        for field in fields {
            let section = field["section"]["label"].as_str().unwrap_or("");
            let label = field["label"].as_str().unwrap_or("");
            let value = field["value"].as_str().unwrap_or("");

            if section == "_Metadata" && label == "version" {
                version = value.parse().unwrap_or(0);
                continue;
            }

            // Skip empty or internal fields
            if section.is_empty() || label.is_empty() || section.starts_with('_') {
                continue;
            }

            // Key is the label, section is the vendor (we don't need vendor in the map)
            secrets.insert(label.to_string(), value.to_string());
        }

        Ok(Self { secrets, version })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn op_item_from_json_parses_secrets() {
        let json = json!({
            "fields": [
                {"section": {"label": "Stripe"}, "label": "API_KEY", "value": "sk_test"},
                {"section": {"label": "Convex"}, "label": "URL", "value": "https://example.com"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.secrets.get("API_KEY").unwrap(), "sk_test");
        assert_eq!(item.secrets.get("URL").unwrap(), "https://example.com");
    }

    #[test]
    fn op_item_from_json_extracts_version() {
        let json = json!({
            "fields": [
                {"section": {"label": "_Metadata"}, "label": "version", "value": "42"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.version, 42);
    }

    #[test]
    fn op_item_from_json_skips_internal_sections() {
        let json = json!({
            "fields": [
                {"section": {"label": "_Internal"}, "label": "hidden", "value": "secret"},
                {"section": {"label": "Stripe"}, "label": "KEY", "value": "val"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.secrets.len(), 1);
        assert!(!item.secrets.contains_key("hidden"));
    }

    #[test]
    fn op_item_from_json_skips_empty_section() {
        let json = json!({
            "fields": [
                {"section": {"label": ""}, "label": "orphan", "value": "val"},
                {"section": {"label": "G"}, "label": "KEY", "value": "v"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.secrets.len(), 1);
        assert!(!item.secrets.contains_key("orphan"));
    }

    #[test]
    fn op_item_from_json_skips_empty_label() {
        let json = json!({
            "fields": [
                {"section": {"label": "G"}, "label": "", "value": "val"},
                {"section": {"label": "G"}, "label": "KEY", "value": "v"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.secrets.len(), 1);
    }

    #[test]
    fn op_item_from_json_no_fields() {
        let json = json!({"title": "item"});
        let err = OpItem::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("no fields"));
    }

    #[test]
    fn op_item_from_json_version_not_numeric() {
        let json = json!({
            "fields": [
                {"section": {"label": "_Metadata"}, "label": "version", "value": "abc"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.version, 0);
    }

    #[test]
    fn op_item_from_json_no_version_field() {
        let json = json!({
            "fields": [
                {"section": {"label": "G"}, "label": "KEY", "value": "v"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.version, 0);
    }

    #[test]
    fn op_item_from_json_empty_values() {
        let json = json!({
            "fields": [
                {"section": {"label": "G"}, "label": "KEY", "value": ""},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.secrets.get("KEY").unwrap(), "");
    }
}
