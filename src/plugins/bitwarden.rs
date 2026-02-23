use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::adapters::{CommandOpts, CommandRunner};
use crate::config::{BitwardenPluginConfig, Config};
use crate::store::StorePayload;

use super::StoragePlugin;

pub struct BitwardenPlugin<'a> {
    config: &'a Config,
    plugin_config: BitwardenPluginConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> BitwardenPlugin<'a> {
    pub fn new(
        config: &'a Config,
        plugin_config: BitwardenPluginConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            plugin_config,
            runner,
        }
    }

    /// Resolve the secret name for an environment.
    fn secret_name(&self, env: &str) -> String {
        self.plugin_config
            .secret_name
            .replace("{project}", &self.config.project)
            .replace("{environment}", env)
    }

    /// List secrets in the project, returning the raw JSON array.
    fn list_secrets(&self) -> Result<Vec<Value>> {
        let project_id = &self.plugin_config.project_id;
        let output = self
            .runner
            .run(
                "bws",
                &[
                    "secret",
                    "list",
                    "--project-id",
                    project_id,
                    "--output",
                    "json",
                ],
                CommandOpts::default(),
            )
            .context("failed to run bws secret list")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("bws secret list failed: {stderr}");
        }

        let items: Vec<Value> =
            serde_json::from_slice(&output.stdout).context("failed to parse bws output")?;
        Ok(items)
    }

    /// Find a secret by name in the list, returning its ID.
    fn find_secret_id(&self, items: &[Value], name: &str) -> Option<String> {
        items.iter().find_map(|item| {
            let item_name = item.get("key")?.as_str()?;
            if item_name == name {
                item.get("id")?.as_str().map(|s| s.to_string())
            } else {
                None
            }
        })
    }
}

impl<'a> StoragePlugin for BitwardenPlugin<'a> {
    fn name(&self) -> &str {
        "bitwarden"
    }

    fn preflight(&self) -> Result<()> {
        crate::adapters::check_command(self.runner, "bws").map_err(|_| {
            anyhow::anyhow!(
                "Bitwarden Secrets Manager CLI (bws) is not installed or not in PATH. Install it from: https://bitwarden.com/help/secrets-manager-cli/"
            )
        })?;

        // Verify auth by listing secrets (requires BWS_ACCESS_TOKEN)
        let project_id = &self.plugin_config.project_id;
        let output = self
            .runner
            .run(
                "bws",
                &["secret", "list", "--project-id", project_id],
                CommandOpts::default(),
            )
            .context("failed to run bws secret list")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Bitwarden authentication failed (is BWS_ACCESS_TOKEN set?): {stderr}");
        }

        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
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

        let version = payload
            .env_versions
            .get(env)
            .copied()
            .unwrap_or(payload.version);

        // Build JSON payload with bare keys + version
        let mut data: BTreeMap<String, Value> = env_secrets
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        data.insert("_esk_version".to_string(), Value::Number(version.into()));

        let json = serde_json::to_string(&data).context("failed to serialize secrets")?;
        let secret_name = self.secret_name(env);

        // Check if the secret already exists
        let items = self.list_secrets()?;
        let existing_id = self.find_secret_id(&items, &secret_name);

        if let Some(id) = existing_id {
            // Update existing secret
            let output = self
                .runner
                .run(
                    "bws",
                    &["secret", "edit", &id, "--value", &json],
                    CommandOpts::default(),
                )
                .context("failed to run bws secret edit")?;
            if !output.success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("bws secret edit failed: {stderr}");
            }
        } else {
            // Create new secret
            let project_id = &self.plugin_config.project_id;
            let output = self
                .runner
                .run(
                    "bws",
                    &[
                        "secret",
                        "create",
                        &secret_name,
                        &json,
                        "--project-id",
                        project_id,
                    ],
                    CommandOpts::default(),
                )
                .context("failed to run bws secret create")?;
            if !output.success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("bws secret create failed: {stderr}");
            }
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let secret_name = self.secret_name(env);
        let items = self.list_secrets()?;

        // Find the secret by name
        let item = match items.iter().find(|item| {
            item.get("key")
                .and_then(|v| v.as_str())
                .map(|s| s == secret_name)
                .unwrap_or(false)
        }) {
            Some(item) => item,
            None => return Ok(None),
        };

        // Parse the value as JSON
        let value_str = item
            .get("value")
            .and_then(|v| v.as_str())
            .context("bws secret has no value field")?;

        let data: BTreeMap<String, Value> =
            serde_json::from_str(value_str).context("failed to parse secret value as JSON")?;

        let mut secrets = BTreeMap::new();
        let mut version = 0u64;

        for (k, v) in &data {
            if k == "_esk_version" {
                version = v
                    .as_u64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0);
                continue;
            }
            let val = v.as_str().unwrap_or_default();
            secrets.insert(format!("{k}:{env}"), val.to_string());
        }

        Ok(Some((secrets, version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{CommandOpts, CommandOutput};
    use serde_json::json;
    use std::sync::Mutex;

    struct MockRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        responses: Mutex<Vec<CommandOutput>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, program: &str, args: &[&str], _opts: CommandOpts) -> Result<CommandOutput> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
            ));
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            } else {
                Ok(responses.remove(0))
            }
        }
    }

    fn make_config(yaml: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        std::mem::forget(dir);
        config
    }

    fn ok_output(stdout: &[u8]) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    #[allow(dead_code)]
    fn fail_output(stderr: &[u8]) -> CommandOutput {
        CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn preflight_success() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();
        let runner = MockRunner::new(vec![ok_output(b"bws 0.4.0"), ok_output(b"[]")]);
        let plugin = BitwardenPlugin::new(&config, plugin_config, &runner);
        assert!(plugin.preflight().is_ok());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert!(calls[1].1.contains(&"secret".to_string()));
    }

    #[test]
    fn preflight_bws_not_installed() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }

        let plugin = BitwardenPlugin::new(&config, plugin_config, &FailRunner);
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("bws) is not installed"));
    }

    #[test]
    fn push_creates_new_secret() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();

        // list returns empty array (no existing secret), then create succeeds
        let runner = MockRunner::new(vec![ok_output(b"[]"), ok_output(b"{}")]);
        let plugin = BitwardenPlugin::new(&config, plugin_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        let payload = StorePayload {
            secrets,
            version: 3,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
        };

        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        // Second call is create
        assert_eq!(calls[1].1[0], "secret");
        assert_eq!(calls[1].1[1], "create");
        assert_eq!(calls[1].1[2], "myapp-dev");
    }

    #[test]
    fn push_updates_existing_secret() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();

        let existing = json!([
            {"id": "secret-456", "key": "myapp-dev", "value": "{}"}
        ]);
        let runner = MockRunner::new(vec![
            ok_output(&serde_json::to_vec(&existing).unwrap()),
            ok_output(b"{}"),
        ]);
        let plugin = BitwardenPlugin::new(&config, plugin_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
        };

        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        // Second call is edit
        assert_eq!(calls[1].1[0], "secret");
        assert_eq!(calls[1].1[1], "edit");
        assert_eq!(calls[1].1[2], "secret-456");
    }

    #[test]
    fn push_skips_empty_env() {
        let yaml = r#"
project: myapp
environments: [dev, prod]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = BitwardenPlugin::new(&config, plugin_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:prod".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
        };

        plugin.push(&payload, &config, "dev").unwrap();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn pull_finds_secret_by_name() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();

        let inner_value =
            json!({"API_KEY": "sk_test", "DB_URL": "postgres://localhost", "_esk_version": 7});
        let items = json!([
            {"id": "s1", "key": "myapp-dev", "value": serde_json::to_string(&inner_value).unwrap()},
            {"id": "s2", "key": "myapp-prod", "value": "{}"}
        ]);
        let runner = MockRunner::new(vec![ok_output(&serde_json::to_vec(&items).unwrap())]);
        let plugin = BitwardenPlugin::new(&config, plugin_config, &runner);

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_not_found_returns_none() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();
        let runner = MockRunner::new(vec![ok_output(b"[]")]);
        let plugin = BitwardenPlugin::new(&config, plugin_config, &runner);

        assert!(plugin.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn secret_name_interpolation() {
        let yaml = r#"
project: myapp
environments: [dev, prod]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();

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

        let plugin = BitwardenPlugin::new(&config, plugin_config, &DummyRunner);
        assert_eq!(plugin.secret_name("dev"), "myapp-dev");
        assert_eq!(plugin.secret_name("prod"), "myapp-prod");
    }

    #[test]
    fn pull_version_as_string() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: BitwardenPluginConfig = config.plugin_config("bitwarden").unwrap();

        let inner_value = json!({"KEY": "val", "_esk_version": "42"});
        let items = json!([
            {"id": "s1", "key": "myapp-dev", "value": serde_json::to_string(&inner_value).unwrap()}
        ]);
        let runner = MockRunner::new(vec![ok_output(&serde_json::to_vec(&items).unwrap())]);
        let plugin = BitwardenPlugin::new(&config, plugin_config, &runner);

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 42);
        assert_eq!(secrets.get("KEY:dev").unwrap(), "val");
    }
}
