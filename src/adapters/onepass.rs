use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::process::Command;

use crate::adapters::SyncAdapter;
use crate::config::{Config, OnePasswordAdapterConfig, ResolvedTarget};

pub struct OnePasswordAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a OnePasswordAdapterConfig,
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

        let output = Command::new("op")
            .args(["item", "get", &item_name, "--vault", vault, "--format", "json"])
            .output()
            .context("failed to run op CLI")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("isn't an item") || stderr.contains("not found") {
                return Ok(None);
            }
            anyhow::bail!("op item get failed: {stderr}");
        }

        let json: Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse op output")?;

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
            let mut cmd = Command::new("op");
            cmd.args(["item", "edit", &item_name, "--vault", vault]);
            for assignment in &assignments {
                cmd.arg(assignment);
            }
            let output = cmd.output().context("failed to run op item edit")?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("op item edit failed: {stderr}");
            }
        } else {
            // Create new item
            let mut cmd = Command::new("op");
            cmd.args([
                "item",
                "create",
                "--category",
                "Secure Note",
                "--title",
                &item_name,
                "--vault",
                vault,
            ]);
            for assignment in &assignments {
                cmd.arg(assignment);
            }
            let output = cmd.output().context("failed to run op item create")?;
            if !output.status.success() {
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

pub struct OpItem {
    pub secrets: BTreeMap<String, String>,
    pub version: u64,
}

impl OpItem {
    fn from_json(json: &Value) -> Result<Self> {
        let mut secrets = BTreeMap::new();
        let mut version = 0u64;

        let fields = json["fields"]
            .as_array()
            .context("op item has no fields")?;

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
