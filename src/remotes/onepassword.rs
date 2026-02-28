//! 1Password remote — syncs secrets via the `op` CLI.
//!
//! 1Password is a password manager with a secrets automation feature.
//! Secrets are stored as items in a vault, with each esk secret mapped to a
//! field on the item. Items are scoped per environment.
//!
//! CLI: `op` (1Password CLI v2).
//! Commands: `op item get` / `op item create` / `op item edit`.
//!
//! The `op` CLI does **not** support stdin for field assignments, so secret
//! values are passed as command-line arguments (visible in `ps` output).
//! Field names are stored with a section prefix for organization. Version
//! metadata is stored as a separate field on the item.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::{Config, OnePasswordRemoteConfig};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct OnePasswordRemote<'a> {
    config: &'a Config,
    remote_config: OnePasswordRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> OnePasswordRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: OnePasswordRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the 1Password item name for an environment.
    pub fn item_name(&self, env: &str) -> String {
        // Capitalize first letter of env for {Environment} pattern
        let env_capitalized = {
            let mut chars = env.chars();
            match chars.next() {
                Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        };

        self.remote_config
            .item_pattern
            .replace("{project}", &self.config.project)
            .replace("{Environment}", &env_capitalized)
            .replace("{environment}", env)
    }

    /// Get a 1Password item, returning None if it doesn't exist.
    pub fn get_item(&self, env: &str) -> Result<Option<OpItem>> {
        let item_name = self.item_name(env);
        let vault = &self.remote_config.vault;

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
    // SECURITY: 1Password CLI (`op item create`/`op item edit`) requires field assignments as
    // positional args (e.g. `section.key[concealed]=value`). There is no stdin/file support for
    // field values. Secret values are exposed in process arguments (visible via `ps aux`).
    // No workaround available.
    pub fn push_item(
        &self,
        env: &str,
        secrets: &BTreeMap<String, String>,
        version: u64,
    ) -> Result<()> {
        let item_name = self.item_name(env);
        let vault = &self.remote_config.vault;

        let existing = self.get_item(env)?;

        // Build field assignments: "group.key[concealed]=value"
        let mut assignments: Vec<String> = Vec::new();

        // Group secrets by group using the config
        let mut by_group: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for (key, value) in secrets {
            let group = self
                .config
                .find_secret(key)
                .map_or_else(|| "General".to_string(), |(g, _)| g);
            by_group
                .entry(group)
                .or_default()
                .push((key.clone(), value.clone()));
        }

        for (group, entries) in &by_group {
            for (key, value) in entries {
                assignments.push(format!("{group}.{key}[concealed]={value}"));
            }
        }

        // Add version metadata
        assignments.push(format!("_Metadata.version[text]={version}"));

        // Remove stale fields from 1Password (present remotely but not locally)
        if let Some(ref item) = existing {
            for remote_key in item.secrets.keys() {
                if !secrets.contains_key(remote_key) {
                    let section = item
                        .sections
                        .get(remote_key)
                        .map_or("General", std::string::String::as_str);
                    assignments.push(format!("{section}.{remote_key}[delete]"));
                }
            }
        }

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
            let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
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
            let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
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
        let Some(item) = self.get_item(env)? else {
            return Ok(None);
        };
        Ok(Some((item.secrets, item.version)))
    }
}

impl SyncRemote for OnePasswordRemote<'_> {
    fn name(&self) -> &'static str {
        "1password"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "op").map_err(|_| {
            anyhow::anyhow!(
                "1Password CLI (op) is not installed or not in PATH. Install it from: https://1password.com/downloads/command-line/"
            )
        })?;
        let vault = &self.remote_config.vault;
        let output = self
            .runner
            .run(
                "op",
                &["vault", "get", vault, "--format", "json"],
                CommandOpts::default(),
            )
            .context("failed to run op vault get")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("1Password vault '{vault}' not accessible: {stderr}");
        }
        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        // Extract bare keys for this environment
        let suffix = format!(":{env}");
        let env_secrets: BTreeMap<String, String> = payload
            .secrets
            .iter()
            .filter_map(|(k, v)| {
                k.strip_suffix(&suffix)
                    .map(|bare| (bare.to_string(), v.clone()))
            })
            .collect();

        if env_secrets.is_empty() {
            return Ok(());
        }

        // Use env-specific version when available, falling back to global
        let version = payload
            .env_versions
            .get(env)
            .copied()
            .unwrap_or(payload.version);
        self.push_item(env, &env_secrets, version)
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        // Pull returns bare keys — convert to composite for consistency
        match self.pull_item(env)? {
            Some((bare_secrets, version)) => {
                let composite: BTreeMap<String, String> = bare_secrets
                    .into_iter()
                    .map(|(k, v)| (format!("{k}:{env}"), v))
                    .collect();
                Ok(Some((composite, version)))
            }
            None => Ok(None),
        }
    }
}

#[derive(Debug)]
pub struct OpItem {
    pub secrets: BTreeMap<String, String>,
    /// Tracks which section each secret key came from (key -> section label).
    pub sections: BTreeMap<String, String>,
    pub version: u64,
}

impl OpItem {
    /// Parse a 1Password item from JSON.
    pub fn from_json(json: &Value) -> Result<Self> {
        let mut secrets = BTreeMap::new();
        let mut sections = BTreeMap::new();
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

            // Key is the label, section is the group
            secrets.insert(label.to_string(), value.to_string());
            sections.insert(label.to_string(), section.to_string());
        }

        Ok(Self {
            secrets,
            sections,
            version,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};
    use serde_json::json;

    type RunnerCall = (String, Vec<String>);

    fn calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .calls()
            .into_iter()
            .map(|call| (call.program, call.args))
            .collect()
    }

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
        assert_eq!(item.sections.get("API_KEY").unwrap(), "Stripe");
        assert_eq!(item.sections.get("URL").unwrap(), "Convex");
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

    #[test]
    fn onepassword_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
            },
        ]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["vault", "get", "V", "--format", "json"]);
    }

    #[test]
    fn onepassword_preflight_vault_inaccessible() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: SecretVault
    item_pattern: test
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"vault not found".to_vec(),
            },
        ]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("1Password vault 'SecretVault' not accessible"));
        assert!(err.to_string().contains("vault not found"));
    }

    #[test]
    fn onepassword_preflight_missing_op() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let runner = ErrorCommandRunner::missing_command();
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("1Password CLI (op) is not installed"));
    }

    #[test]
    fn item_name_substitution() {
        use crate::targets::{CommandOpts, CommandOutput};
        struct DummyRunner;
        impl CommandRunner for DummyRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: "{project} - {Environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = DummyRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.item_name("dev"), "myapp - Dev");
    }

    #[test]
    fn item_name_lowercase() {
        use crate::targets::{CommandOpts, CommandOutput};
        struct DummyRunner;
        impl CommandRunner for DummyRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: "{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = DummyRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.item_name("dev"), "dev");
    }

    #[test]
    fn item_name_empty_env() {
        use crate::targets::{CommandOpts, CommandOutput};
        struct DummyRunner;
        impl CommandRunner for DummyRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: "{project} - {Environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = DummyRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.item_name(""), "myapp - ");
    }

    #[test]
    fn op_item_from_json_tracks_sections() {
        let json = json!({
            "fields": [
                {"section": {"label": "Stripe"}, "label": "API_KEY", "value": "sk_test"},
                {"section": {"label": "AWS"}, "label": "SECRET", "value": "aws_secret"},
            ]
        });
        let item = OpItem::from_json(&json).unwrap();
        assert_eq!(item.sections.len(), 2);
        assert_eq!(item.sections.get("API_KEY").unwrap(), "Stripe");
        assert_eq!(item.sections.get("SECRET").unwrap(), "AWS");
    }

    #[test]
    fn push_item_removes_stale_fields() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
secrets:
  Stripe:
    API_KEY:
      targets: {}
  AWS:
    SECRET:
      targets: {}
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let json = json!({
            "fields": [
                {"section": {"label": "Stripe"}, "label": "API_KEY", "value": "old"},
                {"section": {"label": "AWS"}, "label": "SECRET", "value": "old"},
                {"section": {"label": "Vendor"}, "label": "STALE_KEY", "value": "old"},
                {"section": {"label": "_Metadata"}, "label": "version", "value": "1"},
            ]
        });
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&json).unwrap(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&json).unwrap(),
                stderr: Vec::new(),
            },
        ]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        // Push only API_KEY and SECRET (not STALE_KEY)
        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY".to_string(), "new_val".to_string());
        secrets.insert("SECRET".to_string(), "new_val".to_string());
        remote.push_item("dev", &secrets, 2).unwrap();

        let calls = calls(&runner);
        // Last call is op item edit
        let edit_call = calls.last().unwrap();
        let args_str = edit_call.1.join(" ");
        assert!(args_str.contains("Vendor.STALE_KEY[delete]"));
    }

    #[test]
    fn push_item_no_delete_when_no_stale_fields() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
secrets:
  Stripe:
    API_KEY:
      targets: {}
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let json = json!({
            "fields": [
                {"section": {"label": "Stripe"}, "label": "API_KEY", "value": "old"},
                {"section": {"label": "_Metadata"}, "label": "version", "value": "1"},
            ]
        });
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&json).unwrap(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&json).unwrap(),
                stderr: Vec::new(),
            },
        ]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY".to_string(), "new_val".to_string());
        remote.push_item("dev", &secrets, 2).unwrap();

        let calls = calls(&runner);
        let edit_call = calls.last().unwrap();
        let args_str = edit_call.1.join(" ");
        assert!(!args_str.contains("[delete]"));
    }

    #[test]
    fn push_item_stale_field_uses_remote_section() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let json = json!({
            "fields": [
                {"section": {"label": "Stripe"}, "label": "API_KEY", "value": "old"},
                {"section": {"label": "_Metadata"}, "label": "version", "value": "1"},
            ]
        });
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&json).unwrap(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&json).unwrap(),
                stderr: Vec::new(),
            },
        ]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        // Push with no secrets — API_KEY becomes stale
        let secrets = BTreeMap::new();
        remote.push_item("dev", &secrets, 2).unwrap();

        let calls = calls(&runner);
        let edit_call = calls.last().unwrap();
        let args_str = edit_call.1.join(" ");
        // Should use "Stripe" section from remote, not "General"
        assert!(args_str.contains("Stripe.API_KEY[delete]"));
        assert!(!args_str.contains("General.API_KEY[delete]"));
    }

    #[test]
    fn push_item_create_path_no_delete() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"isn't an item".to_vec(),
            },
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY".to_string(), "val".to_string());
        remote.push_item("dev", &secrets, 1).unwrap();

        let calls = calls(&runner);
        // Second call is op item create
        let create_call = &calls[1];
        let args_str = create_call.1.join(" ");
        assert!(args_str.contains("create"));
        assert!(!args_str.contains("[delete]"));
    }
}
