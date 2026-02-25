//! Azure Key Vault remote — syncs secrets via the `az` CLI.
//!
//! Azure Key Vault is Microsoft's cloud service for securely storing and
//! accessing secrets, encryption keys, and certificates. Secrets are versioned
//! and protected by Azure RBAC or access policies.
//!
//! CLI: `az` (Azure CLI).
//! Commands: `az keyvault secret set --file` / `az keyvault secret show`.
//!
//! Secret values are written to a temp file and passed via `--file` (the `az`
//! CLI does not support stdin for secret values, and using `--value` would
//! expose them in process arguments). Secret names are sanitized to comply with
//! Key Vault naming rules: only alphanumeric characters and hyphens are allowed.
//! Requires a `vault_name` in the config.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;

use crate::targets::{CommandOpts, CommandRunner};
use crate::config::{AzureKeyVaultRemoteConfig, Config};
use crate::store::StorePayload;

use super::SyncRemote;

pub struct AzureKeyVaultRemote<'a> {
    config: &'a Config,
    remote_config: AzureKeyVaultRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> AzureKeyVaultRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: AzureKeyVaultRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the Azure secret name for an environment.
    /// Azure secret names only allow alphanumeric characters and hyphens.
    fn secret_name(&self, env: &str) -> String {
        let raw = self
            .remote_config
            .secret_name
            .replace("{project}", &self.config.project)
            .replace("{environment}", env);

        // Replace non-alphanumeric, non-hyphen characters with hyphens
        raw.chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    }
}

impl<'a> SyncRemote for AzureKeyVaultRemote<'a> {
    fn name(&self) -> &str {
        "azure"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "az").map_err(|_| {
            anyhow::anyhow!(
                "Azure CLI (az) is not installed or not in PATH. Install it from: https://learn.microsoft.com/en-us/cli/azure/install-azure-cli"
            )
        })?;

        let output = self
            .runner
            .run("az", &["account", "show"], CommandOpts::default())
            .context("failed to run az account show")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Azure CLI not authenticated: {stderr}");
        }
        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        let (env_secrets, version) = match super::extract_env_secrets(payload, env) {
            Some(v) => v,
            None => return Ok(()),
        };

        // Build JSON payload with bare keys + version metadata.
        // Write to a temp file and pass via --file to avoid exposing values in process arguments.
        let mut json_map: BTreeMap<String, String> = env_secrets;
        json_map.insert(super::ESK_VERSION_KEY.to_string(), version.to_string());
        let json = serde_json::to_string(&json_map).context("failed to serialize secrets")?;

        let mut tmpfile =
            tempfile::NamedTempFile::new().context("failed to create temp file for azure push")?;
        tmpfile
            .write_all(json.as_bytes())
            .context("failed to write temp file")?;
        let tmppath = tmpfile.path().to_string_lossy().to_string();

        let secret_name = self.secret_name(env);
        let vault_name = &self.remote_config.vault_name;

        let output = self
            .runner
            .run(
                "az",
                &[
                    "keyvault",
                    "secret",
                    "set",
                    "--vault-name",
                    vault_name,
                    "--name",
                    &secret_name,
                    "--file",
                    &tmppath,
                ],
                CommandOpts::default(),
            )
            .context("failed to run az keyvault secret set")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("az keyvault secret set failed: {stderr}");
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let secret_name = self.secret_name(env);
        let vault_name = &self.remote_config.vault_name;

        let output = self
            .runner
            .run(
                "az",
                &[
                    "keyvault",
                    "secret",
                    "show",
                    "--vault-name",
                    vault_name,
                    "--name",
                    &secret_name,
                    "--output",
                    "json",
                ],
                CommandOpts::default(),
            )
            .context("failed to run az keyvault secret show")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("SecretNotFound") {
                return Ok(None);
            }
            anyhow::bail!("az keyvault secret show failed: {stderr}");
        }

        // Parse outer Azure JSON to extract the .value field
        let outer: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("failed to parse az output JSON")?;
        let value_str = outer["value"]
            .as_str()
            .context("az output missing 'value' field")?;

        // Parse the inner JSON (our payload)
        let json_map: BTreeMap<String, String> =
            serde_json::from_str(value_str).context("failed to parse secret value JSON")?;

        Ok(Some(super::parse_pulled_secrets(json_map, env)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

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

    fn azure_yaml() -> &'static str {
        r#"
project: myapp
environments: [dev, prod]
remotes:
  azure:
    vault_name: my-vault
    secret_name: "{project}-{environment}"
"#
    }

    fn make_payload(secrets: &[(&str, &str)], version: u64) -> StorePayload {
        let mut map = BTreeMap::new();
        for (k, v) in secrets {
            map.insert(k.to_string(), v.to_string());
        }
        StorePayload {
            secrets: map,
            version,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        }
    }

    #[test]
    fn secret_name_substitution() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        assert_eq!(remote.secret_name("dev"), "myapp-dev");
        assert_eq!(remote.secret_name("prod"), "myapp-prod");
    }

    #[test]
    fn secret_name_sanitizes_underscores() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: my_app
environments: [dev]
remotes:
  azure:
    vault_name: v
    secret_name: "{project}_{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        std::mem::forget(dir);
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        // Underscores should be replaced with hyphens
        assert_eq!(remote.secret_name("dev"), "my-app-dev");
    }

    #[test]
    fn preflight_success() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.50.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
            },
        ]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert!(calls[1].1.contains(&"account".to_string()));
    }

    #[test]
    fn preflight_missing_az() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("Azure CLI (az)"));
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.50.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"Please run 'az login' to setup account".to_vec(),
            },
        ]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("not authenticated"));
    }

    #[test]
    fn push_success() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test")], 3);
        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "az");
        assert!(calls[0].1.contains(&"keyvault".to_string()));
        assert!(calls[0].1.contains(&"set".to_string()));
        assert!(calls[0].1.contains(&"my-vault".to_string()));
        assert!(calls[0].1.contains(&"myapp-dev".to_string()));
        // Verify --file is used instead of --value (no secret values in args)
        assert!(calls[0].1.contains(&"--file".to_string()));
        assert!(!calls[0].1.contains(&"--value".to_string()));
        // Secret value should not appear in args
        assert!(!calls[0].1.iter().any(|a| a.contains("sk_test")));
    }

    #[test]
    fn push_skips_empty_env() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert!(calls.is_empty());
    }

    #[test]
    fn pull_success() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let inner = serde_json::json!({
            "API_KEY": "sk_test",
            "DB_URL": "postgres://localhost",
            crate::remotes::ESK_VERSION_KEY: "5"
        });
        let outer = serde_json::json!({
            "value": inner.to_string()
        });
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&outer).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        let (secrets, version) = remote.pull(&config, "dev").unwrap().unwrap();

        assert_eq!(version, 5);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_not_found_returns_none() {
        let config = make_config(azure_yaml());
        let remote_config: AzureKeyVaultRemoteConfig = config.remote_config("azure").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"SecretNotFound: secret not found".to_vec(),
        }]);
        let remote = AzureKeyVaultRemote::new(&config, remote_config, &runner);
        assert!(remote.pull(&config, "dev").unwrap().is_none());
    }
}
