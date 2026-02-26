//! Doppler remote — syncs secrets via the `doppler` CLI.
//!
//! Doppler is a secrets management platform designed for developer workflows.
//! Secrets are organized into projects and configs (environments), with
//! automatic syncing to infrastructure and CI/CD.
//!
//! CLI: `doppler` (Doppler's official CLI).
//! Commands: `doppler secrets upload --json` / `doppler secrets download --json`.
//!
//! Secrets are pushed and pulled as JSON objects via **stdin**. Requires a
//! `--project` and `-c <config>` flag for each operation. esk environment names
//! are mapped to Doppler config names via the `config_names` config field.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::config::{Config, DopplerRemoteConfig};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct DopplerRemote<'a> {
    #[allow(dead_code)]
    config: &'a Config,
    remote_config: DopplerRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> DopplerRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: DopplerRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the Doppler config name for an environment.
    fn config_name(&self, env: &str) -> Result<String> {
        self.remote_config
            .config_map
            .get(env)
            .cloned()
            .with_context(|| {
                format!("no Doppler config mapping for environment '{env}' in config_map")
            })
    }
}

impl SyncRemote for DopplerRemote<'_> {
    fn name(&self) -> &'static str {
        "doppler"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "doppler").map_err(|_| {
            anyhow::anyhow!(
                "Doppler CLI (doppler) is not installed or not in PATH. Install it from: https://docs.doppler.com/docs/install-cli"
            )
        })?;

        let output = self
            .runner
            .run("doppler", &["me"], CommandOpts::default())
            .context("failed to run doppler me")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Doppler CLI not authenticated: {stderr}");
        }
        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        let Some((env_secrets, version)) = payload.env_secrets(env) else {
            return Ok(());
        };

        let doppler_config = self.config_name(env)?;
        let project = &self.remote_config.project;

        // Build JSON payload with all secrets + version metadata, upload in a single call
        // via stdin to avoid exposing values in process arguments.
        let mut json_map: BTreeMap<String, String> = env_secrets;
        json_map.insert(super::ESK_VERSION_KEY.to_string(), version.to_string());
        let json = serde_json::to_string(&json_map).context("failed to serialize secrets")?;

        let output = self
            .runner
            .run(
                "doppler",
                &[
                    "secrets",
                    "upload",
                    "--json",
                    "-p",
                    project,
                    "-c",
                    &doppler_config,
                    "--silent",
                ],
                CommandOpts {
                    stdin: Some(json.into_bytes()),
                    ..Default::default()
                },
            )
            .context("failed to run doppler secrets upload")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("doppler secrets upload failed: {stderr}");
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let doppler_config = self.config_name(env)?;
        let project = &self.remote_config.project;

        let output = self
            .runner
            .run(
                "doppler",
                &[
                    "secrets",
                    "download",
                    "-p",
                    project,
                    "-c",
                    &doppler_config,
                    "--format",
                    "json",
                    "--no-file",
                ],
                CommandOpts::default(),
            )
            .context("failed to run doppler secrets download")?;

        if !output.success {
            // Config doesn't exist or other error — treat as not found
            return Ok(None);
        }

        let json_map: BTreeMap<String, String> = serde_json::from_slice(&output.stdout)
            .context("failed to parse Doppler secrets JSON")?;

        // Check both _esk_version (current) and _ESK_VERSION (legacy) for backward compatibility
        let version: u64 = json_map
            .get(super::ESK_VERSION_KEY)
            .or_else(|| json_map.get("_ESK_VERSION"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let composite: BTreeMap<String, String> = json_map
            .into_iter()
            .filter(|(k, _)| k != super::ESK_VERSION_KEY && k != "_ESK_VERSION")
            .map(|(k, v)| (format!("{k}:{env}"), v))
            .collect();

        Ok(Some((composite, version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (String, Vec<String>, Option<Vec<u8>>);

    fn calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.stdin))
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

    fn doppler_yaml() -> &'static str {
        r"
project: myapp
environments: [dev, prod]
remotes:
  doppler:
    project: myapp-doppler
    config_map:
      dev: dev_config
      prod: prd
"
    }

    fn make_payload(secrets: &[(&str, &str)], version: u64) -> StorePayload {
        let mut map = BTreeMap::new();
        for (k, v) in secrets {
            map.insert((*k).to_string(), (*v).to_string());
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
    fn config_name_resolution() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        assert_eq!(remote.config_name("dev").unwrap(), "dev_config");
        assert_eq!(remote.config_name("prod").unwrap(), "prd");
    }

    #[test]
    fn config_name_missing_env() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        let err = remote.config_name("staging").unwrap_err();
        assert!(err.to_string().contains("staging"));
    }

    #[test]
    fn preflight_success() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"v3.60.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: b"user@example.com".to_vec(),
                stderr: Vec::new(),
            },
        ]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["me"]);
    }

    #[test]
    fn preflight_missing_doppler() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("Doppler CLI"));
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn preflight_auth_failure() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"v3.60.0".to_vec(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"Unable to authenticate".to_vec(),
            },
        ]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("not authenticated"));
    }

    #[test]
    fn push_uploads_via_stdin() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        }]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test"), ("DB_URL:dev", "pg://")], 3);
        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.0, "doppler");
        assert_eq!(
            call.1,
            vec![
                "secrets",
                "upload",
                "--json",
                "-p",
                "myapp-doppler",
                "-c",
                "dev_config",
                "--silent"
            ]
        );

        // Verify secrets are passed via stdin, not in args
        let stdin = call.2.as_ref().expect("stdin should be set");
        let parsed: BTreeMap<String, String> = serde_json::from_slice(stdin).unwrap();
        assert_eq!(parsed.get("API_KEY").unwrap(), "sk_test");
        assert_eq!(parsed.get("DB_URL").unwrap(), "pg://");
        assert_eq!(parsed.get("_esk_version").unwrap(), "3");
    }

    #[test]
    fn push_skips_empty_env() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert!(calls.is_empty());
    }

    #[test]
    fn pull_success() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let json = serde_json::json!({
            "API_KEY": "sk_test",
            "DB_URL": "postgres://localhost",
            "_ESK_VERSION": "7"
        });
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&json).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        let (secrets, version) = remote.pull(&config, "dev").unwrap().unwrap();

        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        // Neither version key variant should appear in output
        assert!(!secrets.contains_key("_ESK_VERSION:dev"));
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_not_found_returns_none() {
        let config = make_config(doppler_yaml());
        let remote_config: DopplerRemoteConfig = config.remote_config("doppler").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"config not found".to_vec(),
        }]);
        let remote = DopplerRemote::new(&config, remote_config, &runner);
        assert!(remote.pull(&config, "dev").unwrap().is_none());
    }
}
