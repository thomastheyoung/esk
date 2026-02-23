use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::adapters::{CommandOpts, CommandRunner};
use crate::config::{Config, DopplerPluginConfig};
use crate::store::StorePayload;

use super::StoragePlugin;

pub struct DopplerPlugin<'a> {
    #[allow(dead_code)]
    config: &'a Config,
    plugin_config: DopplerPluginConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> DopplerPlugin<'a> {
    pub fn new(
        config: &'a Config,
        plugin_config: DopplerPluginConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            plugin_config,
            runner,
        }
    }

    /// Resolve the Doppler config name for an environment.
    fn config_name(&self, env: &str) -> Result<String> {
        self.plugin_config
            .config_map
            .get(env)
            .cloned()
            .with_context(|| {
                format!("no Doppler config mapping for environment '{env}' in config_map")
            })
    }
}

impl<'a> StoragePlugin for DopplerPlugin<'a> {
    fn name(&self) -> &str {
        "doppler"
    }

    fn preflight(&self) -> Result<()> {
        crate::adapters::check_command(self.runner, "doppler").map_err(|_| {
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

        let doppler_config = self.config_name(env)?;
        let project = &self.plugin_config.project;

        // Set each secret individually
        for (key, value) in &env_secrets {
            let assignment = format!("{key}={value}");
            let output = self
                .runner
                .run(
                    "doppler",
                    &[
                        "secrets",
                        "set",
                        &assignment,
                        "-p",
                        project,
                        "-c",
                        &doppler_config,
                        "--silent",
                    ],
                    CommandOpts::default(),
                )
                .with_context(|| format!("failed to run doppler secrets set for {key}"))?;
            if !output.success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("doppler secrets set failed for {key}: {stderr}");
            }
        }

        // Set version metadata
        let version_assignment = format!("_ESK_VERSION={version}");
        let output = self
            .runner
            .run(
                "doppler",
                &[
                    "secrets",
                    "set",
                    &version_assignment,
                    "-p",
                    project,
                    "-c",
                    &doppler_config,
                    "--silent",
                ],
                CommandOpts::default(),
            )
            .context("failed to set _ESK_VERSION in Doppler")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("doppler secrets set _ESK_VERSION failed: {stderr}");
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let doppler_config = self.config_name(env)?;
        let project = &self.plugin_config.project;

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

        let version: u64 = json_map
            .get("_ESK_VERSION")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let composite: BTreeMap<String, String> = json_map
            .into_iter()
            .filter(|(k, _)| k != "_ESK_VERSION")
            .map(|(k, v)| (format!("{k}:{env}"), v))
            .collect();

        Ok(Some((composite, version)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{CommandOpts, CommandOutput};
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

    fn doppler_yaml() -> &'static str {
        r#"
project: myapp
environments: [dev, prod]
plugins:
  doppler:
    project: myapp-doppler
    config_map:
      dev: dev_config
      prod: prd
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
    fn config_name_resolution() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = DopplerPlugin::new(&config, plugin_config, &runner);
        assert_eq!(plugin.config_name("dev").unwrap(), "dev_config");
        assert_eq!(plugin.config_name("prod").unwrap(), "prd");
    }

    #[test]
    fn config_name_missing_env() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = DopplerPlugin::new(&config, plugin_config, &runner);
        let err = plugin.config_name("staging").unwrap_err();
        assert!(err.to_string().contains("staging"));
    }

    #[test]
    fn preflight_success() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();
        let runner = MockRunner::new(vec![
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
        let plugin = DopplerPlugin::new(&config, plugin_config, &runner);
        assert!(plugin.preflight().is_ok());
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["me"]);
    }

    #[test]
    fn preflight_missing_doppler() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let plugin = DopplerPlugin::new(&config, plugin_config, &FailRunner);
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("Doppler CLI"));
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn push_sets_each_secret_individually() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();
        // 2 secrets + 1 version = 3 calls, all succeed
        let runner = MockRunner::new(vec![
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]);
        let plugin = DopplerPlugin::new(&config, plugin_config, &runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test"), ("DB_URL:dev", "pg://")], 3);
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        // Each call should use the doppler project and config
        for call in calls.iter() {
            assert_eq!(call.0, "doppler");
            assert!(call.1.contains(&"-p".to_string()));
            assert!(call.1.contains(&"myapp-doppler".to_string()));
            assert!(call.1.contains(&"-c".to_string()));
            assert!(call.1.contains(&"dev_config".to_string()));
        }
        // Last call should be the version
        let last = &calls[2];
        assert!(last.1.contains(&"_ESK_VERSION=3".to_string()));
    }

    #[test]
    fn push_skips_empty_env() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = DopplerPlugin::new(&config, plugin_config, &runner);
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls.lock().unwrap();
        assert!(calls.is_empty());
    }

    #[test]
    fn pull_success() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();
        let json = serde_json::json!({
            "API_KEY": "sk_test",
            "DB_URL": "postgres://localhost",
            "_ESK_VERSION": "7"
        });
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&json).unwrap(),
            stderr: Vec::new(),
        }]);
        let plugin = DopplerPlugin::new(&config, plugin_config, &runner);
        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();

        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_ESK_VERSION:dev"));
    }

    #[test]
    fn pull_not_found_returns_none() {
        let config = make_config(doppler_yaml());
        let plugin_config: DopplerPluginConfig = config.plugin_config("doppler").unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"config not found".to_vec(),
        }]);
        let plugin = DopplerPlugin::new(&config, plugin_config, &runner);
        assert!(plugin.pull(&config, "dev").unwrap().is_none());
    }
}
