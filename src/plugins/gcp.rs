use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::adapters::{CommandOpts, CommandRunner};
use crate::config::{Config, GcpPluginConfig};
use crate::store::StorePayload;

use super::StoragePlugin;

pub struct GcpPlugin<'a> {
    config: &'a Config,
    plugin_config: GcpPluginConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> GcpPlugin<'a> {
    pub fn new(
        config: &'a Config,
        plugin_config: GcpPluginConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            plugin_config,
            runner,
        }
    }

    /// Resolve the GCP secret name for an environment.
    fn secret_name(&self, env: &str) -> String {
        self.plugin_config
            .secret_name
            .replace("{project}", &self.config.project)
            .replace("{environment}", env)
    }
}

impl<'a> StoragePlugin for GcpPlugin<'a> {
    fn name(&self) -> &str {
        "gcp"
    }

    fn preflight(&self) -> Result<()> {
        crate::adapters::check_command(self.runner, "gcloud").map_err(|_| {
            anyhow::anyhow!(
                "Google Cloud CLI (gcloud) is not installed or not in PATH. Install it from: https://cloud.google.com/sdk/docs/install"
            )
        })?;

        let project = &self.plugin_config.gcp_project;
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
        let (env_secrets, version) = match super::extract_env_secrets(payload, env) {
            Some(v) => v,
            None => return Ok(()),
        };

        // Build JSON payload with bare keys + version metadata
        let mut json_map: BTreeMap<String, String> = env_secrets;
        json_map.insert(super::ESK_VERSION_KEY.to_string(), version.to_string());
        let json = serde_json::to_string(&json_map).context("failed to serialize secrets")?;

        let secret_name = self.secret_name(env);
        let project = &self.plugin_config.gcp_project;

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
        let project = &self.plugin_config.gcp_project;

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
    use crate::adapters::{CommandOpts, CommandOutput};
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};
    use std::sync::Mutex;

    type StdinCall = (String, Vec<String>, Option<Vec<u8>>);

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
        // Leak the tempdir so it lives long enough
        std::mem::forget(dir);
        config
    }

    fn gcp_yaml() -> &'static str {
        r#"
project: myapp
environments: [dev, prod]
plugins:
  gcp:
    gcp_project: my-gcp-project
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
        }
    }

    #[test]
    fn secret_name_substitution() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        assert_eq!(plugin.secret_name("dev"), "myapp-dev");
        assert_eq!(plugin.secret_name("prod"), "myapp-prod");
    }

    #[test]
    fn preflight_success() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
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
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        assert!(plugin.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert!(calls[1].1.contains(&"auth".to_string()));
    }

    #[test]
    fn preflight_missing_gcloud() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("gcloud"));
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
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
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("not accessible"));
    }

    #[test]
    fn push_success() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test")], 3);
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "gcloud");
        assert!(calls[0].1.contains(&"versions".to_string()));
        assert!(calls[0].1.contains(&"add".to_string()));
        assert!(calls[0].1.contains(&"myapp-dev".to_string()));
    }

    #[test]
    fn push_creates_secret_on_not_found() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
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
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        let payload = make_payload(&[("KEY:dev", "val")], 1);
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 3);
        assert!(calls[1].1.contains(&"create".to_string()));
    }

    #[test]
    fn push_skips_empty_env() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        // Only prod secrets, pushing dev — should skip
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert!(calls.is_empty());
    }

    #[test]
    fn pull_success() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
        let json = serde_json::json!({
            "API_KEY": "sk_test",
            "DB_URL": "postgres://localhost",
            crate::plugins::ESK_VERSION_KEY: "5"
        });
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&json).unwrap(),
            stderr: Vec::new(),
        }]);
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();

        assert_eq!(version, 5);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_not_found_returns_none() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"NOT_FOUND: Secret not found".to_vec(),
        }]);
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);
        assert!(plugin.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn push_uses_env_version() {
        let config = make_config(gcp_yaml());
        let plugin_config: GcpPluginConfig = config.plugin_config("gcp").unwrap();

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
                self.calls.lock().unwrap().push((
                    program.to_string(),
                    args.iter().map(|s| s.to_string()).collect(),
                    opts.stdin,
                ));
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }
        let runner = StdinCapture {
            calls: Mutex::new(Vec::new()),
        };
        let plugin = GcpPlugin::new(&config, plugin_config, &runner);

        let mut env_versions = BTreeMap::new();
        env_versions.insert("dev".to_string(), 42);
        let payload = StorePayload {
            secrets: BTreeMap::from([("KEY:dev".to_string(), "val".to_string())]),
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions,
        };
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls.lock().unwrap();
        let stdin = calls[0].2.as_ref().unwrap();
        let json: BTreeMap<String, String> = serde_json::from_slice(stdin).unwrap();
        assert_eq!(json.get(crate::plugins::ESK_VERSION_KEY).unwrap(), "42");
    }
}
