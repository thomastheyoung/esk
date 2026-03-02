//! HashiCorp Vault remote — syncs secrets via the `vault` CLI.
//!
//! HashiCorp Vault is an identity-based secrets management system. It supports
//! multiple secrets engines; esk uses the **KV v2** (key-value version 2)
//! engine, which provides versioned secrets with metadata.
//!
//! CLI: `vault` (HashiCorp Vault CLI).
//! Commands: `vault kv put` / `vault kv get` / `vault token lookup`.
//!
//! Secrets are sent via **stdin** as JSON (`-`). The KV path supports
//! `{project}` and `{environment}` placeholders. Requires `VAULT_ADDR` to be
//! set (or configured in the Vault CLI config) and a valid auth token.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::{Config, HashicorpVaultRemoteConfig};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct HashicorpVaultRemote<'a> {
    config: &'a Config,
    remote_config: HashicorpVaultRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> HashicorpVaultRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: HashicorpVaultRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the KV path for an environment.
    fn resolve_path(&self, env: &str) -> String {
        self.remote_config
            .path
            .replace("{project}", &self.config.project)
            .replace("{environment}", env)
    }

    /// Build CommandOpts with VAULT_ADDR if configured.
    fn command_opts(&self) -> CommandOpts {
        let mut opts = CommandOpts::default();
        if let Some(addr) = &self.remote_config.addr {
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

impl SyncRemote for HashicorpVaultRemote<'_> {
    fn name(&self) -> &'static str {
        "vault"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "vault").map_err(|_| {
            anyhow::anyhow!(
                "HashiCorp Vault CLI (vault) is not installed or not in PATH. Install it from: https://developer.hashicorp.com/vault/install"
            )
        })?;

        let output = self
            .runner
            .run("vault", &["token", "lookup"], self.command_opts())
            .context("failed to run vault token lookup")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Vault authentication failed: {stderr}");
        }

        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        let Some((env_secrets, version)) = payload.env_secrets(env) else {
            return Ok(());
        };

        // Build a JSON object with secrets + _esk_version
        let mut data: BTreeMap<String, Value> = env_secrets
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        data.insert(
            super::ESK_VERSION_KEY.to_string(),
            Value::Number(version.into()),
        );

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
        let data = if self.remote_config.kv_version == 2 {
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
            if k == super::ESK_VERSION_KEY {
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
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};
    use serde_json::json;

    type RunnerCall = (String, Vec<String>, Vec<(String, String)>);

    fn calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.env))
            .collect()
    }

    fn make_config(yaml: &str) -> ConfigFixture {
        ConfigFixture::new(yaml).unwrap()
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
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![ok_output(b"vault 1.15.0"), ok_output(b"{}")]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["token", "lookup"]);
    }

    #[test]
    fn preflight_vault_not_installed() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Vault CLI (vault) is not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"vault 1.15.0"),
            fail_output(b"permission denied"),
        ]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("authentication failed"));
    }

    #[test]
    fn push_sends_secrets_with_version() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"")]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        secrets.insert("DB_URL:dev".to_string(), "postgres://localhost".to_string());
        secrets.insert("API_KEY:prod".to_string(), "sk_live".to_string());
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = calls(&runner);
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
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"")]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:dev".to_string(), "val".to_string());
        let mut env_versions = BTreeMap::new();
        env_versions.insert("dev".to_string(), 10);
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions,
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();

        // Verify the stdin payload contains version 10 (env-specific), not 5
        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn push_skips_empty_env() {
        let yaml = r#"
project: myapp
environments: [dev, prod]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:prod".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();
        assert!(calls(&runner).is_empty());
    }

    #[test]
    fn pull_kv_v2_parses_data_data() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
    kv_version: 2
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();

        let response = json!({
            "data": {
                "data": {
                    "API_KEY": "sk_test",
                    "DB_URL": "postgres://localhost",
                    crate::remotes::ESK_VERSION_KEY: 7
                }
            }
        });
        let runner = MockCommandRunner::from_outputs(vec![ok_output(
            &serde_json::to_vec(&response).unwrap(),
        )]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let (secrets, version) = remote.pull(fixture.config(), "dev").unwrap().unwrap();
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
remotes:
  vault:
    path: "secret/{project}/{environment}"
    kv_version: 1
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();

        let response = json!({
            "data": {
                "API_KEY": "sk_test",
                crate::remotes::ESK_VERSION_KEY: 3
            }
        });
        let runner = MockCommandRunner::from_outputs(vec![ok_output(
            &serde_json::to_vec(&response).unwrap(),
        )]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let (secrets, version) = remote.pull(fixture.config(), "dev").unwrap().unwrap();
        assert_eq!(version, 3);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
    }

    #[test]
    fn pull_not_found_returns_none() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![fail_output(
            b"No value found at secret/data/myapp/dev",
        )]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        assert!(remote.pull(fixture.config(), "dev").unwrap().is_none());
    }

    #[test]
    fn pull_auth_error_propagates() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![fail_output(b"permission denied")]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        assert!(err.to_string().contains("permission denied"));
    }

    #[test]
    fn vault_addr_passed_as_env_var() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
    addr: "https://vault.example.com"
"#;
        let fixture = make_config(yaml);
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b""), ok_output(b"")]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        remote.preflight().unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        // First call is --version (check_command), no env vars
        // Second call is token lookup, should have VAULT_ADDR
        assert!(calls[1]
            .2
            .iter()
            .any(|(k, v)| k == "VAULT_ADDR" && v == "https://vault.example.com"));
    }
}
