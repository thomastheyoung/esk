//! AWS Secrets Manager remote — syncs secrets via the `aws` CLI.
//!
//! AWS Secrets Manager is a managed service for storing, rotating, and
//! retrieving secrets (database credentials, API keys, etc.). Unlike SSM
//! Parameter Store, it stores arbitrary blobs with built-in rotation and
//! cross-account access policies.
//!
//! CLI: `aws` (AWS CLI v2).
//! Commands: `aws secretsmanager put-secret-value` / `get-secret-value` / `create-secret`.
//!
//! The entire esk store payload is serialized as a single JSON blob and stored
//! under one secret name per environment (e.g. `{project}/{environment}`).
//! Secret values are sent via **stdin** (`file:///dev/stdin`). On first push,
//! falls back to `create-secret` if the secret doesn't exist yet. Supports
//! `--region` and `--profile` for multi-account setups.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::config::{AwsSecretsManagerRemoteConfig, Config};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct AwsSecretsManagerRemote<'a> {
    config: &'a Config,
    remote_config: AwsSecretsManagerRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> AwsSecretsManagerRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: AwsSecretsManagerRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the secret name for an environment by replacing placeholders.
    pub fn secret_name(&self, env: &str) -> String {
        self.remote_config
            .secret_name
            .replace("{project}", &self.config.project)
            .replace("{environment}", env)
    }

    /// Build base args for --region and --profile flags.
    fn base_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(ref region) = self.remote_config.region {
            args.push("--region".to_string());
            args.push(region.clone());
        }
        if let Some(ref profile) = self.remote_config.profile {
            args.push("--profile".to_string());
            args.push(profile.clone());
        }
        args
    }
}

impl SyncRemote for AwsSecretsManagerRemote<'_> {
    fn name(&self) -> &'static str {
        "aws_secrets_manager"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "aws").map_err(|_| {
            anyhow::anyhow!(
                "AWS CLI (aws) is not installed or not in PATH. Install it from: https://aws.amazon.com/cli/"
            )
        })?;

        let mut args: Vec<String> = vec!["sts".to_string(), "get-caller-identity".to_string()];
        args.extend(self.base_args());
        let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();

        let output = self
            .runner
            .run("aws", &args_ref, CommandOpts::default())
            .context("failed to run aws sts get-caller-identity")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("AWS authentication failed: {stderr}");
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
        let mut env_last_changed_at = BTreeMap::new();
        if let Some(ts) = payload.env_last_changed_at(env) {
            env_last_changed_at.insert(env.to_string(), ts.to_string());
        }

        let env_payload = StorePayload {
            secrets: env_secrets,
            version,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at,
        };

        let json =
            serde_json::to_string(&env_payload).context("failed to serialize store payload")?;

        let secret_name = self.secret_name(env);

        // Try put-secret-value first (update existing)
        let mut args: Vec<String> = vec![
            "secretsmanager".to_string(),
            "put-secret-value".to_string(),
            "--secret-id".to_string(),
            secret_name.clone(),
            "--secret-string".to_string(),
            "file:///dev/stdin".to_string(),
        ];
        args.extend(self.base_args());
        let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();

        let output = self
            .runner
            .run(
                "aws",
                &args_ref,
                CommandOpts {
                    stdin: Some(json.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .context("failed to run aws secretsmanager put-secret-value")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);

            if stderr.contains("ResourceNotFoundException") {
                // Secret doesn't exist yet — create it
                let mut create_args: Vec<String> = vec![
                    "secretsmanager".to_string(),
                    "create-secret".to_string(),
                    "--name".to_string(),
                    secret_name,
                    "--secret-string".to_string(),
                    "file:///dev/stdin".to_string(),
                ];
                create_args.extend(self.base_args());
                let create_ref: Vec<&str> = create_args.iter().map(std::string::String::as_str).collect();

                let create_output = self
                    .runner
                    .run(
                        "aws",
                        &create_ref,
                        CommandOpts {
                            stdin: Some(json.as_bytes().to_vec()),
                            ..Default::default()
                        },
                    )
                    .context("failed to run aws secretsmanager create-secret")?;

                if !create_output.success {
                    let stderr = String::from_utf8_lossy(&create_output.stderr);
                    anyhow::bail!("aws secretsmanager create-secret failed: {stderr}");
                }
            } else {
                anyhow::bail!("aws secretsmanager put-secret-value failed: {stderr}");
            }
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let secret_name = self.secret_name(env);

        let mut args: Vec<String> = vec![
            "secretsmanager".to_string(),
            "get-secret-value".to_string(),
            "--secret-id".to_string(),
            secret_name,
            "--output".to_string(),
            "json".to_string(),
        ];
        args.extend(self.base_args());
        let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();

        let output = self
            .runner
            .run("aws", &args_ref, CommandOpts::default())
            .context("failed to run aws secretsmanager get-secret-value")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("ResourceNotFoundException") {
                return Ok(None);
            }
            anyhow::bail!("aws secretsmanager get-secret-value failed: {stderr}");
        }

        let response: serde_json::Value =
            serde_json::from_slice(&output.stdout).context("failed to parse AWS response")?;

        let secret_string = response["SecretString"]
            .as_str()
            .context("AWS response missing SecretString field")?;

        let remote_payload: StorePayload =
            serde_json::from_str(secret_string).context("failed to parse SecretString payload")?;

        // Convert bare keys back to composite keys
        let composite: BTreeMap<String, String> = remote_payload
            .secrets
            .into_iter()
            .map(|(k, v)| (format!("{k}:{env}"), v))
            .collect();

        Ok(Some((composite, remote_payload.version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{CommandOpts, CommandOutput};
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};
    use serde_json::json;

    type RunnerCall = (String, Vec<String>, Option<Vec<u8>>);

    fn calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.stdin))
            .collect()
    }

    #[test]
    fn secret_name_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

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
        let runner = DummyRunner;
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);
        assert_eq!(remote.secret_name("dev"), "myapp/dev");
        assert_eq!(remote.secret_name("prod"), "myapp/prod");
    }

    #[test]
    fn base_args_with_region_and_profile() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: test
    region: us-west-2
    profile: staging
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

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
        let runner = DummyRunner;
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);
        let args = remote.base_args();
        assert_eq!(args, vec!["--region", "us-west-2", "--profile", "staging"]);
    }

    #[test]
    fn base_args_empty_when_no_options() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: test
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

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
        let runner = DummyRunner;
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);
        assert!(remote.base_args().is_empty());
    }

    #[test]
    fn preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: test
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

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
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["sts", "get-caller-identity"]);
    }

    #[test]
    fn preflight_missing_aws_cli() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: test
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let runner = ErrorCommandRunner::missing_command();
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("AWS CLI (aws) is not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: test
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"aws-cli/2.0.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"Unable to locate credentials".to_vec(),
            },
        ]);
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("AWS authentication failed"));
        assert!(err.to_string().contains("Unable to locate credentials"));
    }

    #[test]
    fn push_creates_secret_on_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr:
                    b"ResourceNotFoundException: Secrets Manager can't find the specified secret."
                        .to_vec(),
            },
            CommandOutput {
                success: true,
                stdout: b"{}".to_vec(),
                stderr: Vec::new(),
            },
        ]);
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);

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
        // First call: put-secret-value
        assert!(calls[0].1.contains(&"put-secret-value".to_string()));
        assert!(calls[0].1.contains(&"myapp/dev".to_string()));
        // Second call: create-secret
        assert!(calls[1].1.contains(&"create-secret".to_string()));
        assert!(calls[1].1.contains(&"myapp/dev".to_string()));
    }

    #[test]
    fn push_updates_existing_secret() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"{}".to_vec(),
            stderr: Vec::new(),
        }]);
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("DB_URL:dev".to_string(), "postgres://localhost".to_string());
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.contains(&"put-secret-value".to_string()));
    }

    #[test]
    fn push_skips_empty_env() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev, prod]
remotes:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);

        // Only prod secrets, push for dev -> should skip
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
    fn pull_success() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let remote_payload = StorePayload {
            secrets: {
                let mut m = BTreeMap::new();
                m.insert("API_KEY".to_string(), "sk_live".to_string());
                m.insert("DB_URL".to_string(), "postgres://prod".to_string());
                m
            },
            version: 7,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };
        let secret_string = serde_json::to_string(&remote_payload).unwrap();
        let aws_response = json!({
            "SecretString": secret_string,
            "Name": "myapp/dev",
        });

        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&aws_response).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);

        let (secrets, version) = remote.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_live");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://prod");
    }

    #[test]
    fn pull_not_found_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"ResourceNotFoundException: Secrets Manager can't find the specified secret."
                .to_vec(),
        }]);
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);

        assert!(remote.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn push_uses_env_version() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  aws_secrets_manager:
    secret_name: "{project}/{environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let remote_config: AwsSecretsManagerRemoteConfig =
            config.remote_config("aws_secrets_manager").unwrap();

        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"{}".to_vec(),
            stderr: Vec::new(),
        }]);
        let remote = AwsSecretsManagerRemote::new(&config, remote_config, &runner);

        let mut env_versions = BTreeMap::new();
        env_versions.insert("dev".to_string(), 10);
        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:dev".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 5,
            tombstones: BTreeMap::new(),
            env_versions,
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
        let pushed: StorePayload = serde_json::from_slice(calls[0].2.as_ref().unwrap()).unwrap();
        assert_eq!(pushed.version, 10);
        assert!(pushed.secrets.contains_key("KEY"));
        assert!(!pushed.secrets.contains_key("KEY:dev"));
    }
}
