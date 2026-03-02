//! GCP Secret Manager remote — syncs secrets via the `gcloud` CLI.
//!
//! Google Cloud Secret Manager is a managed service for storing API keys,
//! passwords, certificates, and other sensitive data. Secrets are versioned
//! (each update creates a new immutable version) and integrate with IAM for
//! access control.
//!
//! CLI: `gcloud` (Google Cloud CLI).
//! Commands: `gcloud secrets versions add --data-file=-` / `gcloud secrets versions access latest`.
//!
//! The entire esk store payload is serialized as JSON and pushed as a new
//! secret version via **stdin** (`--data-file=-`). On first push, creates the
//! secret if it doesn't exist. Supports `--project` for GCP project targeting.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::config::{Config, GcpSecretManagerRemoteConfig};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct GcpSecretManagerRemote<'a> {
    config: &'a Config,
    remote_config: GcpSecretManagerRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> GcpSecretManagerRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: GcpSecretManagerRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the GCP secret name for an environment.
    fn secret_name(&self, env: &str) -> String {
        self.remote_config
            .secret_name
            .replace("{project}", &self.config.project)
            .replace("{environment}", env)
    }
}

impl SyncRemote for GcpSecretManagerRemote<'_> {
    fn name(&self) -> &'static str {
        "gcp"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "gcloud").map_err(|_| {
            anyhow::anyhow!(
                "Google Cloud CLI (gcloud) is not installed or not in PATH. Install it from: https://cloud.google.com/sdk/docs/install"
            )
        })?;

        let project = &self.remote_config.project;
        let output = self
            .runner
            .run(
                "gcloud",
                &["auth", "print-access-token", "--project", project],
                CommandOpts::default(),
            )
            .context("failed to run gcloud auth check")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("GCP project '{project}' not accessible: {stderr}");
        }
        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        let Some((env_secrets, version)) = payload.env_secrets(env) else {
            return Ok(());
        };

        // Build JSON payload with bare keys + version metadata
        let mut json_map: BTreeMap<String, String> = env_secrets;
        json_map.insert(super::ESK_VERSION_KEY.to_string(), version.to_string());
        let json = serde_json::to_string(&json_map).context("failed to serialize secrets")?;

        let secret_name = self.secret_name(env);
        let project = &self.remote_config.project;

        // Try to add a new version
        let output = self
            .runner
            .run(
                "gcloud",
                &[
                    "secrets",
                    "versions",
                    "add",
                    &secret_name,
                    "--data-file=-",
                    "--project",
                    project,
                ],
                CommandOpts {
                    stdin: Some(json.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .context("failed to run gcloud secrets versions add")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("NOT_FOUND") {
                // Secret doesn't exist — create it first
                let create_output = self
                    .runner
                    .run(
                        "gcloud",
                        &["secrets", "create", &secret_name, "--project", project],
                        CommandOpts::default(),
                    )
                    .context("failed to run gcloud secrets create")?;
                if !create_output.success {
                    let err = String::from_utf8_lossy(&create_output.stderr);
                    anyhow::bail!("gcloud secrets create failed: {err}");
                }

                // Retry versions add
                let retry_output = self
                    .runner
                    .run(
                        "gcloud",
                        &[
                            "secrets",
                            "versions",
                            "add",
                            &secret_name,
                            "--data-file=-",
                            "--project",
                            project,
                        ],
                        CommandOpts {
                            stdin: Some(json.as_bytes().to_vec()),
                            ..Default::default()
                        },
                    )
                    .context("failed to run gcloud secrets versions add (retry)")?;
                if !retry_output.success {
                    let err = String::from_utf8_lossy(&retry_output.stderr);
                    anyhow::bail!("gcloud secrets versions add failed: {err}");
                }
            } else {
                anyhow::bail!("gcloud secrets versions add failed: {stderr}");
            }
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let secret_name = self.secret_name(env);
        let project = &self.remote_config.project;

        let output = self
            .runner
            .run(
                "gcloud",
                &[
                    "secrets",
                    "versions",
                    "access",
                    "latest",
                    &format!("--secret={secret_name}"),
                    "--project",
                    project,
                ],
                CommandOpts::default(),
            )
            .context("failed to run gcloud secrets versions access")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("NOT_FOUND") {
                return Ok(None);
            }
            anyhow::bail!("gcloud secrets versions access failed: {stderr}");
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let json_map: BTreeMap<String, String> =
            serde_json::from_str(&stdout).context("failed to parse GCP secret JSON")?;

        Ok(Some(super::parse_pulled_secrets(json_map, env)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{CommandOpts, CommandOutput};
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};
    use std::sync::Mutex;

    type StdinCall = (String, Vec<String>, Option<Vec<u8>>);




    fn gcp_yaml() -> &'static str {
        r#"
project: myapp
environments: [dev, prod]
remotes:
  gcp:
    project: my-gcp-project
    secret_name: "{project}-{environment}"
"#
    }

    fn make_payload(secrets: &[(&str, &str)], version: u64) -> StorePayload {
        let mut map = BTreeMap::new();
        for (k, v) in secrets {
            map.insert((*k).to_string(), (*v).to_string());
        }
        StorePayload {
            secrets: map,
            version,
            ..Default::default()
        }
    }

    #[test]
    fn secret_name_substitution() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        assert_eq!(remote.secret_name("dev"), "myapp-dev");
        assert_eq!(remote.secret_name("prod"), "myapp-prod");
    }

    #[test]
    fn preflight_success() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"gcloud 400.0.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: b"ya29.token".to_vec(),
                stderr: Vec::new(),
            },
        ]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert!(calls[1].args.contains(&"auth".to_string()));
    }

    #[test]
    fn preflight_missing_gcloud() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("gcloud"));
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"gcloud 400.0.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"ERROR: (gcloud.auth.print-access-token) not authenticated".to_vec(),
            },
        ]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("not accessible"));
    }

    #[test]
    fn push_success() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test")], 3);
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "gcloud");
        assert!(calls[0].args.contains(&"versions".to_string()));
        assert!(calls[0].args.contains(&"add".to_string()));
        assert!(calls[0].args.contains(&"myapp-dev".to_string()));
    }

    #[test]
    fn push_creates_secret_on_not_found() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // First versions add fails with NOT_FOUND
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"NOT_FOUND: Secret not found".to_vec(),
            },
            // secrets create succeeds
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            // Retry versions add succeeds
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        let payload = make_payload(&[("KEY:dev", "val")], 1);
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 3);
        assert!(calls[1].args.contains(&"create".to_string()));
    }

    #[test]
    fn push_skips_empty_env() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        // Only prod secrets, pushing dev — should skip
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls();
        assert!(calls.is_empty());
    }

    #[test]
    fn pull_success() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let json = serde_json::json!({
            "API_KEY": "sk_test",
            "DB_URL": "postgres://localhost",
            crate::remotes::ESK_VERSION_KEY: "5"
        });
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&json).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        let (secrets, version) = remote.pull(fixture.config(), "dev").unwrap().unwrap();

        assert_eq!(version, 5);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_not_found_returns_none() {
        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"NOT_FOUND: Secret not found".to_vec(),
        }]);
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);
        assert!(remote.pull(fixture.config(), "dev").unwrap().is_none());
    }

    #[test]
    fn push_uses_env_version() {
        // Capture stdin to verify version
        struct StdinCapture {
            calls: Mutex<Vec<StdinCall>>,
        }
        impl CommandRunner for StdinCapture {
            fn run(
                &self,
                program: &str,
                args: &[&str],
                opts: CommandOpts,
            ) -> Result<CommandOutput> {
                self.calls.lock().expect("stdin capture mutex poisoned").push((
                    program.to_string(),
                    args.iter().map(|s| (*s).to_string()).collect(),
                    opts.stdin,
                ));
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let fixture = ConfigFixture::new(gcp_yaml()).expect("fixture");
        let remote_config: GcpSecretManagerRemoteConfig =
            fixture.config().remote_config("gcp").unwrap();
        let runner = StdinCapture {
            calls: Mutex::new(Vec::new()),
        };
        let remote = GcpSecretManagerRemote::new(fixture.config(), remote_config, &runner);

        let mut env_versions = BTreeMap::new();
        env_versions.insert("dev".to_string(), 42);
        let payload = StorePayload {
            secrets: BTreeMap::from([("KEY:dev".to_string(), "val".to_string())]),
            version: 1,
            env_versions,
            ..Default::default()
        };
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls.lock().expect("stdin capture mutex poisoned");
        let stdin = calls[0].2.as_ref().unwrap();
        let json: BTreeMap<String, String> = serde_json::from_slice(stdin).unwrap();
        assert_eq!(json.get(crate::remotes::ESK_VERSION_KEY).unwrap(), "42");
    }
}
