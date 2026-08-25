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
        crate::targets::check_command(self.runner, "vault")
            .context("Install from: https://developer.hashicorp.com/vault/install")?;

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
        let Some(flat) = super::flat_snapshot_map(payload, env)? else {
            return Ok(());
        };

        // Build a JSON object with secrets + _esk_version
        let data: BTreeMap<String, Value> = flat
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();

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

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<super::RemoteSnapshot>> {
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

        // KV v2: data is at .data.data, KV v1: data is at .data.
        //
        // KV v2 soft-delete keeps the version in storage but stops returning its
        // payload: the CLI still exits 0 with an HTTP 200 body shaped like
        // `{"data":{"data":null,"metadata":{"deletion_time":"...","destroyed":false}}}`.
        // That is `Some(Value::Null)`, not absent, so it must be distinguished from
        // both a malformed response (key missing) and a genuinely non-object value.
        //
        // We deliberately do NOT map this to `Ok(None)` ("not found"): sync.rs turns
        // a `None` pull into a version-0 empty snapshot, which reconcile.rs treats as
        // behind local and queues for push — silently writing secrets back into a
        // path the user just soft-deleted. Failing loudly with a specific message
        // keeps today's fail-safe behavior while fixing the misleading error text.
        let data = if self.remote_config.kv_version == 2 {
            let inner = json
                .get("data")
                .context("missing .data.data in vault KV v2 response")?;
            let data_data = inner.get("data");
            match data_data {
                None => anyhow::bail!("missing .data.data in vault KV v2 response"),
                Some(Value::Null) => {
                    let deletion_time = inner
                        .get("metadata")
                        .and_then(|m| m.get("deletion_time"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty());
                    let destroyed = inner
                        .get("metadata")
                        .and_then(|m| m.get("destroyed"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if destroyed {
                        anyhow::bail!(
                            "vault secret at '{path}' has been permanently destroyed; \
                             restore it from backup or `vault kv put` a new version"
                        );
                    }
                    match deletion_time {
                        Some(time) => anyhow::bail!(
                            "vault secret at '{path}' was soft-deleted at {time}; \
                             run `vault kv undelete` to restore it or `vault kv destroy` \
                             to permanently remove it before syncing"
                        ),
                        None => anyhow::bail!(
                            "vault secret at '{path}' has no data (soft-deleted or never \
                             written); run `vault kv undelete` to restore it if it was \
                             soft-deleted"
                        ),
                    }
                }
                Some(other) => other,
            }
        } else {
            let data = json
                .get("data")
                .context("missing .data in vault KV v1 response")?;
            match data {
                Value::Null => {
                    anyhow::bail!(
                        "vault secret at '{path}' has no data (deleted or never written)"
                    );
                }
                other => other,
            }
        };

        let obj = data
            .as_object()
            .context("vault data is not a JSON object")?;

        let strings: BTreeMap<String, String> = obj
            .iter()
            .map(|(key, value)| -> Result<_> {
                let value = if key == super::ESK_VERSION_KEY {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .or_else(|| value.as_u64().map(|v| v.to_string()))
                        .context("Vault version metadata has invalid type")?
                } else {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .context("Vault snapshot contains a non-string value")?
                };
                Ok((key.clone(), value))
            })
            .collect::<Result<_>>()?;
        Ok(Some(super::parse_flat_snapshot(strings, env)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};
    use serde_json::json;

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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![ok_output(b"vault 1.15.0"), ok_output(b"{}")]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["token", "lookup"]);
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("Install from:"));
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
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
            ..Default::default()
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args[0], "kv");
        assert_eq!(calls[0].args[1], "put");
        assert_eq!(calls[0].args[2], "secret/data/myapp/dev");
        assert_eq!(calls[0].args[3], "-");
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
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
            env_versions,
            ..Default::default()
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();

        // Verify the stdin payload contains version 10 (env-specific), not 5
        let calls = runner.calls();
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:prod".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            ..Default::default()
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();
        assert!(runner.calls().is_empty());
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
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

        let snapshot = remote.pull(fixture.config(), "dev").unwrap().unwrap();
        let secrets = snapshot.secrets;
        let version = snapshot.version;
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
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

        let snapshot = remote.pull(fixture.config(), "dev").unwrap().unwrap();
        let secrets = snapshot.secrets;
        let version = snapshot.version;
        assert_eq!(version, 3);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
    }

    // Note: the "normal populated response" success path is already covered by
    // `pull_kv_v2_parses_data_data` (KV v2) and `pull_kv_v1_parses_data` (KV v1) above.

    #[test]
    fn pull_kv_v2_soft_deleted_errors_with_deletion_time() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
    kv_version: 2
"#;
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();

        let response = json!({
            "data": {
                "data": null,
                "metadata": {
                    "deletion_time": "2026-08-20T12:34:56.789Z",
                    "destroyed": false
                }
            }
        });
        let runner = MockCommandRunner::from_outputs(vec![ok_output(
            &serde_json::to_vec(&response).unwrap(),
        )])
        .strict();
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("soft-deleted"),
            "expected message to mention soft-deletion, got: {msg}"
        );
        assert!(
            msg.contains("2026-08-20T12:34:56.789Z"),
            "expected message to include deletion_time, got: {msg}"
        );
        assert!(
            msg.contains("secret/data/myapp/dev"),
            "expected message to include the path, got: {msg}"
        );
        assert!(
            !msg.contains("not a JSON object"),
            "message should not fall through to the generic non-object error, got: {msg}"
        );
    }

    #[test]
    fn pull_kv_v2_destroyed_errors_distinctly() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
    kv_version: 2
"#;
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();

        let response = json!({
            "data": {
                "data": null,
                "metadata": {
                    "deletion_time": "2026-08-20T12:34:56.789Z",
                    "destroyed": true
                }
            }
        });
        let runner = MockCommandRunner::from_outputs(vec![ok_output(
            &serde_json::to_vec(&response).unwrap(),
        )])
        .strict();
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("destroyed"),
            "expected message to mention destruction, got: {msg}"
        );
        assert!(
            !msg.contains("not a JSON object"),
            "message should not fall through to the generic non-object error, got: {msg}"
        );
    }

    #[test]
    fn pull_kv_v1_null_data_errors_distinctly() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/{project}/{environment}"
    kv_version: 1
"#;
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();

        let response = json!({ "data": null });
        let runner = MockCommandRunner::from_outputs(vec![ok_output(
            &serde_json::to_vec(&response).unwrap(),
        )])
        .strict();
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("no data"),
            "expected message to mention missing data, got: {msg}"
        );
        assert!(
            msg.contains("secret/myapp/dev"),
            "expected message to include the path, got: {msg}"
        );
        assert!(
            !msg.contains("not a JSON object"),
            "message should not fall through to the generic non-object error, got: {msg}"
        );
    }

    #[test]
    fn pull_kv_v2_missing_data_data_key_errors_unchanged() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
    kv_version: 2
"#;
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();

        // `.data` present, but `.data.data` key entirely absent (malformed response).
        let response = json!({
            "data": {
                "metadata": {
                    "deletion_time": "",
                    "destroyed": false
                }
            }
        });
        let runner = MockCommandRunner::from_outputs(vec![ok_output(
            &serde_json::to_vec(&response).unwrap(),
        )])
        .strict();
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        assert!(err
            .to_string()
            .contains("missing .data.data in vault KV v2 response"));
    }

    #[test]
    fn pull_kv_v2_non_object_data_data_errors_unchanged() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  vault:
    path: "secret/data/{project}/{environment}"
    kv_version: 2
"#;
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();

        // `.data.data` present but a non-object, non-null value.
        let response = json!({
            "data": {
                "data": "unexpected-string",
                "metadata": {
                    "deletion_time": "",
                    "destroyed": false
                }
            }
        });
        let runner = MockCommandRunner::from_outputs(vec![ok_output(
            &serde_json::to_vec(&response).unwrap(),
        )])
        .strict();
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        assert!(err.to_string().contains("vault data is not a JSON object"));
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: HashicorpVaultRemoteConfig =
            fixture.config().remote_config("vault").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b""), ok_output(b"")]);
        let remote = HashicorpVaultRemote::new(fixture.config(), remote_config, &runner);

        remote.preflight().unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        // First call is --version (check_command), no env vars
        // Second call is token lookup, should have VAULT_ADDR
        assert!(calls[1]
            .env
            .iter()
            .any(|(k, v)| k == "VAULT_ADDR" && v == "https://vault.example.com"));
    }
}
