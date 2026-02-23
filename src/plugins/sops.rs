use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::adapters::{CommandOpts, CommandRunner};
use crate::config::{Config, SopsPluginConfig};
use crate::store::StorePayload;

use super::StoragePlugin;

pub struct SopsPlugin<'a> {
    #[allow(dead_code)]
    config: &'a Config,
    plugin_config: SopsPluginConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> SopsPlugin<'a> {
    pub fn new(
        config: &'a Config,
        plugin_config: SopsPluginConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            plugin_config,
            runner,
        }
    }

    /// Resolve the file path for an environment.
    fn resolve_path(&self, env: &str) -> String {
        self.plugin_config
            .path
            .replace("{environment}", env)
    }
}

impl<'a> StoragePlugin for SopsPlugin<'a> {
    fn name(&self) -> &str {
        "sops"
    }

    fn preflight(&self) -> Result<()> {
        crate::adapters::check_command(self.runner, "sops").map_err(|_| {
            anyhow::anyhow!(
                "Mozilla SOPS (sops) is not installed or not in PATH. Install it from: https://github.com/getsops/sops"
            )
        })?;
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

        // Build JSON payload with bare keys + version metadata
        let mut json_map: BTreeMap<String, String> = env_secrets;
        json_map.insert("_esk_version".to_string(), version.to_string());
        let json = serde_json::to_string_pretty(&json_map)
            .context("failed to serialize secrets")?;

        let dest_path = self.resolve_path(env);

        // Encrypt via sops using stdin, capture stdout
        let output = self
            .runner
            .run(
                "sops",
                &["-e", "/dev/stdin"],
                CommandOpts {
                    stdin: Some(json.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .context("failed to run sops encrypt")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("sops encrypt failed: {stderr}");
        }

        // Write encrypted output to destination path atomically
        if let Some(parent) = std::path::Path::new(&dest_path).parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        let dir = std::path::Path::new(&dest_path)
            .parent()
            .context("path has no parent")?;
        let tmp = tempfile::NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), &output.stdout)?;
        tmp.persist(&dest_path)
            .with_context(|| format!("failed to write {dest_path}"))?;

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let file_path = self.resolve_path(env);

        if !std::path::Path::new(&file_path).exists() {
            return Ok(None);
        }

        // Decrypt via sops
        let output = self
            .runner
            .run(
                "sops",
                &["-d", &file_path],
                CommandOpts::default(),
            )
            .context("failed to run sops decrypt")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("sops decrypt failed: {stderr}");
        }

        let json_map: BTreeMap<String, String> = serde_json::from_slice(&output.stdout)
            .context("failed to parse decrypted SOPS JSON")?;

        let version: u64 = json_map
            .get("_esk_version")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let composite: BTreeMap<String, String> = json_map
            .into_iter()
            .filter(|(k, _)| k != "_esk_version")
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

    fn sops_yaml() -> &'static str {
        r#"
project: myapp
environments: [dev, prod]
plugins:
  sops:
    path: "secrets/{environment}.enc.json"
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
    fn resolve_path_substitution() {
        let config = make_config(sops_yaml());
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = SopsPlugin::new(&config, plugin_config, &runner);
        assert_eq!(plugin.resolve_path("dev"), "secrets/dev.enc.json");
        assert_eq!(plugin.resolve_path("prod"), "secrets/prod.enc.json");
    }

    #[test]
    fn preflight_success() {
        let config = make_config(sops_yaml());
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: b"sops 3.8.0".to_vec(),
            stderr: Vec::new(),
        }]);
        let plugin = SopsPlugin::new(&config, plugin_config, &runner);
        assert!(plugin.preflight().is_ok());
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec!["--version"]);
    }

    #[test]
    fn preflight_missing_sops() {
        let config = make_config(sops_yaml());
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let plugin = SopsPlugin::new(&config, plugin_config, &FailRunner);
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("SOPS"));
        assert!(err.to_string().contains("not installed"));
    }

    #[test]
    fn push_encrypts_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("secrets/dev.enc.json");
        let yaml = format!(
            r#"
project: myapp
environments: [dev]
plugins:
  sops:
    path: "{}/secrets/{{environment}}.enc.json"
"#,
            dir.path().display()
        );
        let config = make_config(&yaml);
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();

        let encrypted = b"ENCRYPTED_CONTENT";
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: encrypted.to_vec(),
            stderr: Vec::new(),
        }]);
        let plugin = SopsPlugin::new(&config, plugin_config, &runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test")], 3);
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "sops");
        assert_eq!(calls[0].1, vec!["-e", "/dev/stdin"]);

        // Verify file was written
        assert!(dest.exists());
        let content = std::fs::read(&dest).unwrap();
        assert_eq!(content, encrypted);
    }

    #[test]
    fn push_skips_empty_env() {
        let config = make_config(sops_yaml());
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = SopsPlugin::new(&config, plugin_config, &runner);
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        plugin.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls.lock().unwrap();
        assert!(calls.is_empty());
    }

    #[test]
    fn pull_decrypts_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("secrets/dev.enc.json");
        std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
        std::fs::write(&file_path, b"encrypted").unwrap();

        let yaml = format!(
            r#"
project: myapp
environments: [dev]
plugins:
  sops:
    path: "{}/secrets/{{environment}}.enc.json"
"#,
            dir.path().display()
        );
        let config = make_config(&yaml);
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();

        let decrypted = serde_json::json!({
            "API_KEY": "sk_test",
            "DB_URL": "postgres://localhost",
            "_esk_version": "5"
        });
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&decrypted).unwrap(),
            stderr: Vec::new(),
        }]);
        let plugin = SopsPlugin::new(&config, plugin_config, &runner);
        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();

        assert_eq!(version, 5);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_missing_file_returns_none() {
        let config = make_config(sops_yaml());
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();
        let runner = MockRunner::new(vec![]);
        let plugin = SopsPlugin::new(&config, plugin_config, &runner);
        // Path "secrets/dev.enc.json" doesn't exist
        assert!(plugin.pull(&config, "dev").unwrap().is_none());

        let calls = runner.calls.lock().unwrap();
        assert!(calls.is_empty());
    }

    #[test]
    fn push_uses_env_version() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: myapp
environments: [dev]
plugins:
  sops:
    path: "{}/secrets/{{environment}}.enc.json"
"#,
            dir.path().display()
        );
        let config = make_config(&yaml);
        let plugin_config: SopsPluginConfig = config.plugin_config("sops").unwrap();

        // Capture stdin to verify version
        struct StdinCapture {
            calls: Mutex<Vec<(String, Vec<String>, Option<Vec<u8>>)>>,
        }
        impl CommandRunner for StdinCapture {
            fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput> {
                self.calls.lock().unwrap().push((
                    program.to_string(),
                    args.iter().map(|s| s.to_string()).collect(),
                    opts.stdin,
                ));
                Ok(CommandOutput {
                    success: true,
                    stdout: b"encrypted".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }
        let runner = StdinCapture {
            calls: Mutex::new(Vec::new()),
        };
        let plugin = SopsPlugin::new(&config, plugin_config, &runner);

        let mut env_versions = BTreeMap::new();
        env_versions.insert("dev".to_string(), 99);
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
        assert_eq!(json.get("_esk_version").unwrap(), "99");
    }
}
