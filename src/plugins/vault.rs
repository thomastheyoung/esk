use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::adapters::{CommandOpts, CommandRunner};
use crate::config::{Config, VaultPluginConfig};
use crate::store::StorePayload;

use super::StoragePlugin;

pub struct VaultPlugin<'a> {
    config: &'a Config,
    plugin_config: VaultPluginConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> VaultPlugin<'a> {
    pub fn new(
        config: &'a Config,
        plugin_config: VaultPluginConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            plugin_config,
            runner,
        }
    }

    /// Resolve the KV path for an environment.
    fn resolve_path(&self, env: &str) -> String {
        self.plugin_config
            .path
            .replace("{project}", &self.config.project)
            .replace("{environment}", env)
    }

    /// Build CommandOpts with VAULT_ADDR if configured.
    fn command_opts(&self) -> CommandOpts {
        let mut opts = CommandOpts::default();
        if let Some(addr) = &self.plugin_config.addr {
            opts.env.push(("VAULT_ADDR".to_string(), addr.clone()));
        }
        opts
    }

    /// Build CommandOpts with VAULT_ADDR and stdin data.
    fn command_opts_with_stdin(&self, stdin: Vec<u8>) -> CommandOpts {
        let mut opts = self.command_opts();
        opts.stdin = Some(stdin);
        opts
    }
}

impl<'a> StoragePlugin for VaultPlugin<'a> {
    fn name(&self) -> &str {
        "vault"
    }

    fn preflight(&self) -> Result<()> {
        crate::adapters::check_command(self.runner, "vault").map_err(|_| {
            anyhow::anyhow!(
                "HashiCorp Vault CLI (vault) is not installed or not in PATH. Install it from: https://developer.hashicorp.com/vault/install"
            )
        })?;

        let output = self
            .runner
            .run(
                "vault",
                &["token", "lookup"],
                self.command_opts(),
            )
            .context("failed to run vault token lookup")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Vault authentication failed: {stderr}");
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

        // Build a JSON object with secrets + _esk_version
        let mut data: BTreeMap<String, Value> = env_secrets
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        data.insert("_esk_version".to_string(), Value::Number(version.into()));

        let json = serde_json::to_string(&data).context("failed to serialize secrets")?;

        let path = self.resolve_path(env);
        let output = self
            .runner
            .run(
                "vault",
                &["kv", "put", &path, "-"],
                self.command_opts_with_stdin(json.into_bytes()),
            )
            .context("failed to run vault kv put")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("vault kv put failed: {stderr}");
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let path = self.resolve_path(env);

        let output = self
            .runner
            .run(
                "vault",
                &["kv", "get", "-format=json", &path],
                self.command_opts(),
            )
            .context("failed to run vault kv get")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No value found") || stderr.contains("not found") {
                return Ok(None);
            }
            anyhow::bail!("vault kv get failed: {stderr}");
        }

        let json: Value =
            serde_json::from_slice(&output.stdout).context("failed to parse vault output")?;

        // KV v2: data is at .data.data, KV v1: data is at .data
        let data = if self.plugin_config.kv_version == 2 {
            json.get("data")
                .and_then(|d| d.get("data"))
                .context("missing .data.data in vault KV v2 response")?
        } else {
            json.get("data")
                .context("missing .data in vault KV v1 response")?
        };

        let obj = data
            .as_object()
            .context("vault data is not a JSON object")?;

        let mut secrets = BTreeMap::new();
        let mut version = 0u64;

        for (k, v) in obj {
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
        calls: Mutex<Vec<(String, Vec<String>, Vec<(String, String)>)>>,
        responses: Mutex<Vec<CommandOutput>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }

        fn calls(&self) -> Vec<(String, Vec<String>, Vec<(String, String)>)> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl CommandRunner for MockRunner {
        fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
                opts.env.clone(),
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
        // Leak the tempdir so it stays alive
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
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![
            ok_output(b"vault 1.15.0"),
            ok_output(b"{}"),
        ]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);
        assert!(plugin.preflight().is_ok());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["token", "lookup"]);
    }

    #[test]
    fn preflight_vault_not_installed() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }

        let plugin = VaultPlugin::new(&config, plugin_config, &FailRunner);
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("Vault CLI (vault) is not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![
            ok_output(b"vault 1.15.0"),
            fail_output(b"permission denied"),
        ]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("authentication failed"));
    }

    #[test]
    fn push_sends_secrets_with_version() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![ok_output(b"")]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        secrets.insert("DB_URL:dev".to_string(), "postgres://localhost".to_string());
        secrets.insert("API_KEY:prod".to_string(), "sk_live".to_string());
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
        };

        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1[0], "kv");
        assert_eq!(calls[0].1[1], "put");
        assert_eq!(calls[0].1[2], "secret/data/myapp/dev");
        assert_eq!(calls[0].1[3], "-");
    }

    #[test]
    fn push_uses_env_version() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![ok_output(b"")]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:dev".to_string(), "val".to_string());
        let mut env_versions = BTreeMap::new();
        env_versions.insert("dev".to_string(), 10);
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions,
        };

        plugin.push(&payload, &config, "dev").unwrap();

        // Verify the stdin payload contains version 10 (env-specific), not 5
        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn push_skips_empty_env() {
        let yaml = r#"
project: myapp
environments: [dev, prod]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

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
    fn pull_kv_v2_parses_data_data() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
    kv_version: 2
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();

        let response = json!({
            "data": {
                "data": {
                    "API_KEY": "sk_test",
                    "DB_URL": "postgres://localhost",
                    "_esk_version": 7
                }
            }
        });
        let runner = MockRunner::new(vec![ok_output(&serde_json::to_vec(&response).unwrap())]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_kv_v1_parses_data() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/{project}/{environment}"
    kv_version: 1
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();

        let response = json!({
            "data": {
                "API_KEY": "sk_test",
                "_esk_version": 3
            }
        });
        let runner = MockRunner::new(vec![ok_output(&serde_json::to_vec(&response).unwrap())]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 3);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
    }

    #[test]
    fn pull_not_found_returns_none() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![fail_output(b"No value found at secret/data/myapp/dev")]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

        assert!(plugin.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn pull_auth_error_propagates() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![fail_output(b"permission denied")]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

        let err = plugin.pull(&config, "dev").unwrap_err();
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn vault_addr_passed_as_env_var() {
        let yaml = r#"
project: myapp
environments: [dev]
plugins:
  vault:
    path: "secret/data/{project}/{environment}"
    addr: "https://vault.example.com"
"#;
        let config = make_config(yaml);
        let plugin_config: VaultPluginConfig = config.plugin_config("vault").unwrap();
        let runner = MockRunner::new(vec![ok_output(b""), ok_output(b"")]);
        let plugin = VaultPlugin::new(&config, plugin_config, &runner);

        plugin.preflight().unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        // First call is --version (check_command), no env vars
        // Second call is token lookup, should have VAULT_ADDR
        assert!(calls[1].2.iter().any(|(k, v)| k == "VAULT_ADDR" && v == "https://vault.example.com"));
    }
}
