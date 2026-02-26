//! Bitwarden Secrets Manager remote — syncs secrets via the `bws` CLI.
//!
//! Bitwarden Secrets Manager is a secrets management service separate from the
//! Bitwarden password manager. It is designed for machine-to-machine secrets
//! (API keys, certificates, database credentials) with project-scoped access.
//!
//! CLI: `bws` (Bitwarden Secrets Manager CLI, distinct from the `bw` vault CLI).
//! Commands: `bws secret list --project-id` / `bws secret create` / `bws secret update`.
//!
//! Requires a `BWS_ACCESS_TOKEN` environment variable for authentication.
//! Secrets are scoped to a project ID and stored one-per-secret (not as a
//! single blob). The esk version metadata and store payload are stored as a
//! specially-named secret within the project.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::{BitwardenRemoteConfig, Config};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct BitwardenRemote<'a> {
    config: &'a Config,
    remote_config: BitwardenRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> BitwardenRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: BitwardenRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the secret name for an environment.
    fn secret_name(&self, env: &str) -> String {
        self.remote_config
            .secret_name
            .replace("{project}", &self.config.project)
            .replace("{environment}", env)
    }

    /// List secrets in the project, returning the raw JSON array.
    fn list_secrets(&self) -> Result<Vec<Value>> {
        let project_id = &self.remote_config.project_id;
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
    #[allow(clippy::unused_self)]
    fn find_secret_id(&self, items: &[Value], name: &str) -> Option<String> {
        items.iter().find_map(|item| {
            let item_name = item.get("key")?.as_str()?;
            if item_name == name {
                item.get("id")?
                    .as_str()
                    .map(std::string::ToString::to_string)
            } else {
                None
            }
        })
    }
}

impl SyncRemote for BitwardenRemote<'_> {
    fn name(&self) -> &'static str {
        "bitwarden"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "bws").map_err(|_| {
            anyhow::anyhow!(
                "Bitwarden Secrets Manager CLI (bws) is not installed or not in PATH. Install it from: https://bitwarden.com/help/secrets-manager-cli/"
            )
        })?;

        // Verify auth by listing secrets (requires BWS_ACCESS_TOKEN)
        let project_id = &self.remote_config.project_id;
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
        let Some((env_secrets, version)) = payload.env_secrets(env) else {
            return Ok(());
        };

        // Build JSON payload with bare keys + version
        let mut data: BTreeMap<String, Value> = env_secrets
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        data.insert(
            super::ESK_VERSION_KEY.to_string(),
            Value::Number(version.into()),
        );

        let json = serde_json::to_string(&data).context("failed to serialize secrets")?;
        let secret_name = self.secret_name(env);

        // Check if the secret already exists
        let items = self.list_secrets()?;
        let existing_id = self.find_secret_id(&items, &secret_name);

        // SECURITY: `bws` CLI requires `--value` as an argument for both `secret edit` and
        // `secret create`. There is no stdin/file support. Secret values are exposed in process
        // arguments (visible via `ps aux`). No workaround available.
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
            let project_id = &self.remote_config.project_id;
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
        let Some(item) = items.iter().find(|item| {
            item.get("key")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == secret_name)
        }) else {
            return Ok(None);
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
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![ok_output(b"bws 0.4.0"), ok_output(b"[]")]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert!(calls[1].1.contains(&"secret".to_string()));
    }

    #[test]
    fn preflight_bws_not_installed() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = BitwardenRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("bws) is not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"bws 0.4.0"),
            fail_output(b"Unauthorized"),
        ]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("Bitwarden authentication failed"));
    }

    #[test]
    fn push_creates_new_secret() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();

        // list returns empty array (no existing secret), then create succeeds
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"[]"), ok_output(b"{}")]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        let payload = StorePayload {
            secrets,
            version: 3,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
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
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();

        let existing = json!([
            {"id": "secret-456", "key": "myapp-dev", "value": "{}"}
        ]);
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(&serde_json::to_vec(&existing).unwrap()),
            ok_output(b"{}"),
        ]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
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
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:prod".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();
        assert!(calls(&runner).is_empty());
    }

    #[test]
    fn pull_finds_secret_by_name() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();

        let inner_value = json!({"API_KEY": "sk_test", "DB_URL": "postgres://localhost", crate::remotes::ESK_VERSION_KEY: 7});
        let items = json!([
            {"id": "s1", "key": "myapp-dev", "value": serde_json::to_string(&inner_value).unwrap()},
            {"id": "s2", "key": "myapp-prod", "value": "{}"}
        ]);
        let runner =
            MockCommandRunner::from_outputs(vec![ok_output(&serde_json::to_vec(&items).unwrap())]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);

        let (secrets, version) = remote.pull(&config, "dev").unwrap().unwrap();
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
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"[]")]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);

        assert!(remote.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn secret_name_interpolation() {
        let yaml = r#"
project: myapp
environments: [dev, prod]
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();

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

        let remote = BitwardenRemote::new(&config, remote_config, &DummyRunner);
        assert_eq!(remote.secret_name("dev"), "myapp-dev");
        assert_eq!(remote.secret_name("prod"), "myapp-prod");
    }

    #[test]
    fn pull_version_as_string() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  bitwarden:
    project_id: "proj-123"
    secret_name: "{project}-{environment}"
"#;
        let config = make_config(yaml);
        let remote_config: BitwardenRemoteConfig = config.remote_config("bitwarden").unwrap();

        let inner_value = json!({"KEY": "val", crate::remotes::ESK_VERSION_KEY: "42"});
        let items = json!([
            {"id": "s1", "key": "myapp-dev", "value": serde_json::to_string(&inner_value).unwrap()}
        ]);
        let runner =
            MockCommandRunner::from_outputs(vec![ok_output(&serde_json::to_vec(&items).unwrap())]);
        let remote = BitwardenRemote::new(&config, remote_config, &runner);

        let (secrets, version) = remote.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 42);
        assert_eq!(secrets.get("KEY:dev").unwrap(), "val");
    }
}
