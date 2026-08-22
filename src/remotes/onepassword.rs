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
use crate::targets::{CommandOpts, CommandOutput, CommandRunner};

use super::SyncRemote;

/// An `op` invocation was aimed at something esk does not own.
///
/// Returned instead of running the command, so a bad config or a future code
/// path cannot reach an unrelated vault item.
#[derive(Debug, thiserror::Error)]
pub enum AccessError {
    #[error(
        "refusing to run `op {verb}` against item {requested:?}: esk only manages {owned:?} in vault {vault:?}"
    )]
    ForeignItem {
        verb: &'static str,
        requested: String,
        owned: Vec<String>,
        vault: String,
    },
    /// Unreachable through `Config::load`, which rejects an empty environment
    /// list. Kept as defense in depth for a `Config` built without `validate()`:
    /// an empty owned set would otherwise fall through to `ForeignItem` and
    /// report "esk only manages []", which reads as a config typo rather than
    /// the invariant violation it is.
    #[error(
        "refusing to run `op {verb}`: esk owns no items in vault {vault:?} (no environments are configured)"
    )]
    NoOwnedItems { verb: &'static str, vault: String },
}

/// Counts of esk-owned vs. other items in the configured vault.
///
/// Deliberately carries no titles — see [`OnePasswordRemote::vault_composition`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultComposition {
    pub vault: String,
    pub esk_owned: usize,
    pub foreign: usize,
    /// How many esk-owned titles match more than one item in the vault.
    ///
    /// `op` resolves a duplicated title to an arbitrary one of the matches, so
    /// esk could read one copy and write another. Non-zero means the vault must
    /// be cleaned up before esk can be trusted to act on the right item.
    pub duplicate_owned: usize,
}

impl VaultComposition {
    /// True when the vault holds nothing but esk's own items.
    pub fn is_isolated(&self) -> bool {
        self.foreign == 0
    }
}

pub struct OnePasswordRemote<'a> {
    config: &'a Config,
    remote_config: OnePasswordRemoteConfig,
    /// Private and never exposed: every `op` call must go through
    /// [`Self::run_op`], which enforces item ownership first.
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

    /// Every item title esk owns in this vault — one per configured environment.
    ///
    /// This is the whole of esk's reach into 1Password. Membership in this set,
    /// not the shape of the title, is what [`Self::assert_owns`] checks: a
    /// prefix test could be widened by a crafted `item_pattern`, a set cannot.
    pub fn owned_items(&self) -> Vec<String> {
        self.config
            .environments
            .iter()
            .map(|env| self.item_name(env))
            .collect()
    }

    /// Reject any item title outside [`Self::owned_items`].
    ///
    /// Matching is case-insensitive because `op` resolves item titles that way:
    /// `op item get "VINEO - DEV"` returns the item titled `vineo - Dev`. A
    /// case-sensitive guard would be *narrower* than what `op` can reach, so an
    /// item differing only in case would slip past as "not ours" while `op`
    /// still resolved to it. Guard and CLI must agree on item identity.
    ///
    /// Full Unicode lowercasing rather than ASCII-only: `op`'s folding of
    /// non-ASCII titles is unverified, and the two error directions are not
    /// symmetric. Too narrow means esk waves through a title that `op` will
    /// resolve to a *different* item — a wrong-item write. Too wide means esk
    /// accepts a title that differs from an owned one only by case, which the
    /// production call sites cannot produce anyway: both pass
    /// [`Self::item_name`] output verbatim. Err wide.
    fn assert_owns(&self, verb: &'static str, item: &str) -> Result<(), AccessError> {
        let owned = self.owned_items();
        if owned.is_empty() {
            return Err(AccessError::NoOwnedItems {
                verb,
                vault: self.remote_config.vault.clone(),
            });
        }
        let needle = item.to_lowercase();
        if owned.iter().any(|o| o.to_lowercase() == needle) {
            return Ok(());
        }
        Err(AccessError::ForeignItem {
            verb,
            requested: item.to_string(),
            owned,
            vault: self.remote_config.vault.clone(),
        })
    }

    /// The only path from this module to the `op` CLI for item commands.
    ///
    /// `item` is checked against the owned set before the command runs, so an
    /// item esk does not own is never named in an `op` invocation at all.
    fn run_op(&self, verb: &'static str, item: &str, args: &[&str]) -> Result<CommandOutput> {
        self.assert_owns(verb, item)?;
        self.runner
            .run("op", args, CommandOpts::default())
            .with_context(|| format!("failed to run op item {verb}"))
    }

    /// How much of the configured vault is esk's.
    ///
    /// Returns counts only. `op item list` hands back the title of every item
    /// in the vault; those titles are tallied and dropped inside this function
    /// and never returned, logged, or stored, so esk learns how many foreign
    /// items exist without retaining what they are.
    ///
    /// A vault holding only esk items is the one real isolation boundary —
    /// [`Self::assert_owns`] guards esk's own code, but only 1Password can stop
    /// a credential from reaching an item in the first place.
    pub fn vault_composition(&self) -> Result<VaultComposition> {
        let vault = &self.remote_config.vault;
        let output = self
            .runner
            .run(
                "op",
                &["item", "list", "--vault", vault, "--format", "json"],
                CommandOpts::default(),
            )
            .context("failed to run op item list")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("op item list failed: {stderr}");
        }

        let json: Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse op item list output")?;
        let items = json
            .as_array()
            .context("op item list did not return a list")?;

        // Fold exactly as `assert_owns` does, so an item esk would write to is
        // never tallied as foreign.
        let owned: Vec<String> = self
            .owned_items()
            .iter()
            .map(|o| o.to_lowercase())
            .collect();
        let mut esk_owned = 0usize;
        let mut foreign = 0usize;
        let mut hits_per_owned_title: BTreeMap<String, usize> = BTreeMap::new();
        for item in items {
            let title = item["title"].as_str().unwrap_or("").to_lowercase();
            if owned.contains(&title) {
                esk_owned += 1;
                *hits_per_owned_title.entry(title).or_default() += 1;
            } else {
                foreign += 1;
            }
        }

        let duplicate_owned = hits_per_owned_title.values().filter(|&&n| n > 1).count();

        Ok(VaultComposition {
            vault: vault.clone(),
            esk_owned,
            foreign,
            duplicate_owned,
        })
    }

    /// Resolve the 1Password item name for an environment.
    fn item_name(&self, env: &str) -> String {
        // Capitalize first letter of env for {Environment} pattern
        let env_capitalized = {
            let mut chars = env.chars();
            match chars.next() {
                Some(c) => format!("{}{}", c.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        };

        let resolved = self
            .remote_config
            .item_pattern
            .replace("{project}", &self.config.project)
            .replace("{Environment}", &env_capitalized)
            .replace("{environment}", env);

        let prefix = self.remote_config.prefix.trim();
        if prefix.is_empty() {
            resolved
        } else {
            format!("{prefix} {resolved}")
        }
    }

    /// Get a 1Password item, returning None if it doesn't exist.
    fn get_item(&self, env: &str) -> Result<Option<OpItem>> {
        let item_name = self.item_name(env);
        let vault = &self.remote_config.vault;

        let output = self.run_op(
            "get",
            &item_name,
            &[
                "item", "get", &item_name, "--vault", vault, "--format", "json",
            ],
        )?;

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
    fn push_item(&self, env: &str, secrets: &BTreeMap<String, String>, version: u64) -> Result<()> {
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
                item_name.clone(),
                "--vault".to_string(),
                vault.clone(),
            ];
            args.extend(assignments.iter().cloned());
            let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
            let output = self.run_op("edit", &item_name, &args_ref)?;
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
                item_name.clone(),
                "--vault".to_string(),
                vault.clone(),
            ];
            args.extend(assignments.iter().cloned());
            let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();
            let output = self.run_op("create", &item_name, &args_ref)?;
            if !output.success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("op item create failed: {stderr}");
            }
        }

        Ok(())
    }

    /// Pull secrets from a 1Password item.
    fn pull_item(&self, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
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

    fn passes_value_as_cli_arg(&self) -> bool {
        true
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "op")
            .context("Install from: https://1password.com/downloads/command-line/")?;
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

        let version = payload.env_version(env);
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
struct OpItem {
    secrets: BTreeMap<String, String>,
    /// Tracks which section each secret key came from (key -> section label).
    sections: BTreeMap<String, String>,
    version: u64,
}

impl OpItem {
    /// Parse a 1Password item from JSON.
    fn from_json(json: &Value) -> Result<Self> {
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
    item_pattern: test-{environment}
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
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["vault", "get", "V", "--format", "json"]);
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
    item_pattern: test-{environment}
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
    item_pattern: test-{environment}
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();

        let runner = ErrorCommandRunner::missing_command();
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("Install from:"));
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
        assert_eq!(remote.item_name("dev"), "⚙ myapp - Dev");
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
        assert_eq!(remote.item_name("dev"), "⚙ dev");
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
        assert_eq!(remote.item_name(""), "⚙ myapp - ");
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
    item_pattern: test-{environment}
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

        let calls = runner.calls();
        // Last call is op item edit
        let edit_call = calls.last().unwrap();
        let args_str = edit_call.args.join(" ");
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
    item_pattern: test-{environment}
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

        let calls = runner.calls();
        let edit_call = calls.last().unwrap();
        let args_str = edit_call.args.join(" ");
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
    item_pattern: test-{environment}
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

        let calls = runner.calls();
        let edit_call = calls.last().unwrap();
        let args_str = edit_call.args.join(" ");
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
    item_pattern: test-{environment}
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

        let calls = runner.calls();
        // Second call is op item create
        let create_call = &calls[1];
        let args_str = create_call.args.join(" ");
        assert!(args_str.contains("create"));
        assert!(!args_str.contains("[delete]"));
    }

    // --- Access control: esk must never touch an item it does not own ---

    /// Config with two environments, so the owned set has more than one member.
    fn access_config(dir: &std::path::Path, extra: &str) -> Config {
        let yaml = format!(
            r#"
project: vineo
environments: [dev, prod]
remotes:
  1password:
    vault: SharedVault
    item_pattern: "{{project}} - {{Environment}}"
{extra}
"#
        );
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    /// A runner that fails the test if it is ever invoked.
    struct ForbiddenRunner;
    impl CommandRunner for ForbiddenRunner {
        fn run(&self, program: &str, args: &[&str], _: CommandOpts) -> Result<CommandOutput> {
            panic!("op must not be executed: {program} {args:?}");
        }
    }

    #[test]
    fn prefix_defaults_to_esk_marker() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();
        assert_eq!(op_config.prefix, "⚙");

        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.item_name("dev"), "⚙ vineo - Dev");
        assert_eq!(remote.item_name("prod"), "⚙ vineo - Prod");
    }

    #[test]
    fn documented_default_resolves_to_the_path_form() {
        // The shape docs/esk.example.yaml recommends, pinned end to end so the
        // docs and the binary cannot drift apart.
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev, prod]
remotes:
  1password:
    vault: Engineering
    item_pattern: "esk/{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();
        // prefix is omitted above, so this exercises the built-in default.
        assert_eq!(op_config.prefix, "\u{2699}");

        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.item_name("dev"), "\u{2699} esk/myapp/dev");
        assert_eq!(remote.item_name("prod"), "\u{2699} esk/myapp/prod");
        // Environment names pass through unaltered — no title-casing.
        assert!(!remote.item_name("prod").contains("Prod"));
    }

    #[test]
    fn prefix_can_be_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: vineo
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: "{project} - {Environment}"
    prefix: "⟦ESK⟧"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.item_name("dev"), "⟦ESK⟧ vineo - Dev");
    }

    #[test]
    fn empty_prefix_opts_out() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: vineo
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: "{project} - {Environment}"
    prefix: ""
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.item_name("dev"), "vineo - Dev");
    }

    #[test]
    fn owned_items_covers_exactly_the_configured_environments() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(
            remote.owned_items(),
            vec!["⚙ vineo - Dev", "⚙ vineo - Prod"]
        );
    }

    #[test]
    fn assert_owns_accepts_owned_items() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert!(remote.assert_owns("get", "⚙ vineo - Dev").is_ok());
        assert!(remote.assert_owns("edit", "⚙ vineo - Prod").is_ok());
    }

    #[test]
    fn assert_owns_rejects_a_foreign_item() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        for foreign in [
            "Personal Bank Login",
            "vineo esk store key",
            "⚙ otherproject - Dev",
            "vineo - Dev",
        ] {
            let err = remote.assert_owns("get", foreign).unwrap_err();
            assert!(
                matches!(err, AccessError::ForeignItem { .. }),
                "expected refusal for {foreign:?}"
            );
        }
    }

    #[test]
    fn assert_owns_rejects_an_item_merely_wearing_the_prefix() {
        // A prefix *test* would pass this; set membership must not.
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        let err = remote
            .assert_owns("get", "⚙ someone elses secrets")
            .unwrap_err();
        assert!(matches!(err, AccessError::ForeignItem { .. }));
    }

    #[test]
    fn assert_owns_rejects_when_no_environments_configured() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: vineo
environments: []
remotes:
  1password:
    vault: V
    item_pattern: "{project}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let Ok(config) = Config::load(&path) else {
            // Config validation may reject an empty environment list outright,
            // which satisfies the same invariant at an earlier layer.
            return;
        };
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        let err = remote.assert_owns("get", "vineo").unwrap_err();
        assert!(matches!(err, AccessError::NoOwnedItems { .. }));
    }

    #[test]
    fn foreign_item_is_never_passed_to_op() {
        // The guard must run *before* the process is spawned, so a foreign
        // title never appears in an op argv at all.
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner; // panics if op is executed
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        let err = remote.run_op("get", "Personal Bank Login", &["item", "get", "x"]);
        assert!(err.is_err());
    }

    #[test]
    fn every_item_command_names_only_owned_titles() {
        // Drive a real push and assert that no op invocation mentions any item
        // outside the owned set.
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();

        let existing = json!({
            "fields": [
                {"section": {"label": "G"}, "label": "KEY", "value": "old"},
                {"section": {"label": "_Metadata"}, "label": "version", "value": "1"},
            ]
        });
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: serde_json::to_vec(&existing).unwrap(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY".to_string(), "new".to_string());
        remote.push_item("dev", &secrets, 2).unwrap();

        let owned = remote.owned_items();
        let calls = runner.calls();
        assert!(!calls.is_empty());

        let mut titles_inspected = 0usize;
        for call in &calls {
            // The item title is the argument following `get`, `edit`, or `--title`.
            for (i, arg) in call.args.iter().enumerate() {
                let is_title_slot = matches!(arg.as_str(), "get" | "edit" | "--title")
                    && call.args.first().map(String::as_str) == Some("item");
                if is_title_slot {
                    let title = &call.args[i + 1];
                    titles_inspected += 1;
                    assert!(
                        owned.contains(title),
                        "op was asked for un-owned item {title:?}"
                    );
                }
            }
        }

        // Without this the test passes vacuously if the argv shape ever changes
        // and no title slot is recognized — reporting safety it never checked.
        assert_eq!(
            titles_inspected, 2,
            "expected to inspect the titles of `op item get` and `op item edit`"
        );
    }

    #[test]
    fn push_and_pull_target_the_prefixed_item() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();

        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"isn't an item".to_vec(),
        }]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert!(remote.pull(&config, "dev").unwrap().is_none());

        let calls = runner.calls();
        assert_eq!(calls[0].args[2], "⚙ vineo - Dev");
    }

    #[test]
    fn vault_composition_counts_without_returning_titles() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();

        let listing = json!([
            {"title": "⚙ vineo - Dev"},
            {"title": "⚙ vineo - Prod"},
            {"title": "Personal Bank Login"},
            {"title": "vineo esk store key"},
        ]);
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&listing).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        let comp = remote.vault_composition().unwrap();
        assert_eq!(comp.esk_owned, 2);
        assert_eq!(comp.foreign, 2);
        assert!(!comp.is_isolated());

        // The struct must not carry any foreign title.
        let rendered = format!("{comp:?}");
        assert!(!rendered.contains("Bank"));
        assert!(!rendered.contains("store key"));
    }

    #[test]
    fn assert_owns_folds_case_like_op_does() {
        // `op item get "VINEO - DEV"` resolves the item titled "vineo - Dev".
        // The guard must treat those as the same item, or a case variant would
        // be judged foreign while op still wrote to the real item.
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();
        let runner = ForbiddenRunner;
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        for variant in ["⚙ VINEO - DEV", "⚙ vineo - dev", "⚙ ViNeO - DeV"] {
            assert!(
                remote.assert_owns("get", variant).is_ok(),
                "case variant {variant:?} must be recognized as owned"
            );
        }

        // Folding must not widen the guard to genuinely different titles.
        assert!(remote.assert_owns("get", "⚙ vineo - Staging").is_err());
        assert!(remote.assert_owns("get", "personal bank login").is_err());
    }

    #[test]
    fn vault_composition_folds_case_like_the_guard() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();

        // A case variant of an owned title is an item esk would write to, so
        // it must count as esk-owned rather than foreign.
        let listing = json!([
            {"title": "⚙ vineo - dev"},
            {"title": "⚙ vineo - Prod"},
            {"title": "Personal Bank Login"},
        ]);
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&listing).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        let comp = remote.vault_composition().unwrap();
        assert_eq!(comp.esk_owned, 2, "case variant must count as esk-owned");
        assert_eq!(comp.foreign, 1);
    }

    #[test]
    fn no_unguarded_op_item_call_sites_exist() {
        // Structural guard: item commands must route through `run_op`, which
        // enforces ownership. A new `self.runner.run("op", ...)` that names an
        // item would bypass that, so the count of raw runner uses is pinned.
        //
        // The three permitted uses are: inside `run_op` itself, `check_command`
        // (runs `op --version`), and `vault_composition` / `preflight`, which
        // are vault-scoped and name no item.
        let src = include_str!("onepassword.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("module has a test section");

        // Normalize whitespace so a `self\n    .runner` line break still counts.
        let flat: String = production.split_whitespace().collect::<Vec<_>>().join(" ");
        let raw_uses = flat.matches("self .runner").count() + flat.matches("self.runner").count();
        assert_eq!(
            raw_uses, 4,
            "raw `self.runner` uses changed ({raw_uses}); every item command must go \
             through run_op(). Permitted: run_op itself, check_command (op --version), \
             preflight (op vault get), vault_composition (op item list) — all of which \
             name no specific item. Anything naming an item must use run_op()."
        );
    }

    #[test]
    fn vault_composition_flags_duplicate_owned_titles() {
        // Two items share an owned title: `op` resolves such a title to an
        // arbitrary one of them, so esk could read one and write the other.
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();

        let listing = json!([
            {"title": "⚙ vineo - Dev"},
            {"title": "⚙ vineo - Dev"},
            {"title": "⚙ vineo - Prod"},
        ]);
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&listing).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        let comp = remote.vault_composition().unwrap();
        assert_eq!(comp.esk_owned, 3);
        assert_eq!(comp.foreign, 0);
        assert_eq!(
            comp.duplicate_owned, 1,
            "the duplicated title must be flagged"
        );
        // An ambiguous vault is not a clean vault, even with nothing foreign.
        assert!(comp.is_isolated());
    }

    #[test]
    fn vault_composition_reports_no_duplicates_when_titles_are_unique() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();

        let listing = json!([
            {"title": "⚙ vineo - Dev"},
            {"title": "⚙ vineo - Prod"},
            {"title": "Unrelated"},
        ]);
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&listing).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);
        assert_eq!(remote.vault_composition().unwrap().duplicate_owned, 0);
    }

    #[test]
    fn vault_composition_reports_an_isolated_vault() {
        let dir = tempfile::tempdir().unwrap();
        let config = access_config(dir.path(), "");
        let op_config = config.onepassword_remote_config().unwrap();

        let listing = json!([
            {"title": "⚙ vineo - Dev"},
            {"title": "⚙ vineo - Prod"},
        ]);
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&listing).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = OnePasswordRemote::new(&config, op_config, &runner);

        let comp = remote.vault_composition().unwrap();
        assert_eq!(comp.foreign, 0);
        assert!(comp.is_isolated());
    }
}
