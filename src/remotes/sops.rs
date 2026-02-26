//! Mozilla SOPS remote — syncs secrets via the `sops` CLI.
//!
//! SOPS (Secrets OPerationS) is a file encryption tool that encrypts the
//! *values* in structured files (JSON, YAML, ENV) while leaving keys in
//! cleartext. Supports multiple key backends: AGE, PGP, AWS KMS, GCP KMS,
//! Azure Key Vault, and HashiCorp Vault Transit.
//!
//! CLI: `sops` (Mozilla SOPS).
//! Commands: `sops -e /dev/stdin` (encrypt) / `sops -d /dev/stdin` (decrypt).
//!
//! The esk store payload is serialized as JSON, encrypted via **stdin**, and
//! written to a file (one per environment). On pull, the file is decrypted via
//! stdin. Requires a `.sops.yaml` configuration file to define encryption keys
//! and rules.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::config::{Config, SopsRemoteConfig};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct SopsRemote<'a> {
    #[allow(dead_code)]
    config: &'a Config,
    remote_config: SopsRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> SopsRemote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: SopsRemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Resolve the file path for an environment.
    fn resolve_path(&self, env: &str) -> String {
        self.remote_config.path.replace("{environment}", env)
    }
}

impl SyncRemote for SopsRemote<'_> {
    fn name(&self) -> &'static str {
        "sops"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "sops").map_err(|_| {
            anyhow::anyhow!(
                "Mozilla SOPS (sops) is not installed or not in PATH. Install it from: https://github.com/getsops/sops"
            )
        })?;

        let sops_config = self.config.root.join(".sops.yaml");
        if !sops_config.exists() {
            anyhow::bail!(
                "SOPS config (.sops.yaml) not found at {}. Create it with encryption rules or set SOPS key environment variables (SOPS_AGE_KEY_FILE, SOPS_PGP_FP, etc.).",
                sops_config.display()
            );
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
        let json =
            serde_json::to_string_pretty(&json_map).context("failed to serialize secrets")?;

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
            .run("sops", &["-d", &file_path], CommandOpts::default())
            .context("failed to run sops decrypt")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("sops decrypt failed: {stderr}");
        }

        let json_map: BTreeMap<String, String> = serde_json::from_slice(&output.stdout)
            .context("failed to parse decrypted SOPS JSON")?;

        Ok(Some(super::parse_pulled_secrets(json_map, env)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{CommandOpts, CommandOutput};
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
        std::mem::forget(dir);
        config
    }

    fn sops_yaml() -> &'static str {
        r#"
project: myapp
environments: [dev, prod]
remotes:
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
            env_last_changed_at: BTreeMap::new(),
        }
    }

    #[test]
    fn resolve_path_substitution() {
        let config = make_config(sops_yaml());
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = SopsRemote::new(&config, remote_config, &runner);
        assert_eq!(remote.resolve_path("dev"), "secrets/dev.enc.json");
        assert_eq!(remote.resolve_path("prod"), "secrets/prod.enc.json");
    }

    /// Create a config in a specific directory so we can also place .sops.yaml there.
    fn make_config_in(dir: &std::path::Path, yaml: &str) -> Config {
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    #[test]
    fn preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        // Create .sops.yaml in the project root
        std::fs::write(
            dir.path().join(".sops.yaml"),
            "creation_rules:\n  - age: age1xxx\n",
        )
        .unwrap();
        let config = make_config_in(dir.path(), sops_yaml());
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"sops 3.8.0".to_vec(),
            stderr: Vec::new(),
        }]);
        let remote = SopsRemote::new(&config, remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1, vec!["--version"]);
    }

    #[test]
    fn preflight_missing_sops_config() {
        // Config root has no .sops.yaml
        let config = make_config(sops_yaml());
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"sops 3.8.0".to_vec(),
            stderr: Vec::new(),
        }]);
        let remote = SopsRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains(".sops.yaml"));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn preflight_missing_sops() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".sops.yaml"),
            "creation_rules:\n  - age: age1xxx\n",
        )
        .unwrap();
        let config = make_config_in(dir.path(), sops_yaml());
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = SopsRemote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
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
remotes:
  sops:
    path: "{}/secrets/{{environment}}.enc.json"
"#,
            dir.path().display()
        );
        let config = make_config(&yaml);
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();

        let encrypted = b"ENCRYPTED_CONTENT";
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: encrypted.to_vec(),
            stderr: Vec::new(),
        }]);
        let remote = SopsRemote::new(&config, remote_config, &runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test")], 3);
        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
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
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = SopsRemote::new(&config, remote_config, &runner);
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
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
remotes:
  sops:
    path: "{}/secrets/{{environment}}.enc.json"
"#,
            dir.path().display()
        );
        let config = make_config(&yaml);
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();

        let decrypted = serde_json::json!({
            "API_KEY": "sk_test",
            "DB_URL": "postgres://localhost",
            crate::remotes::ESK_VERSION_KEY: "5"
        });
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: serde_json::to_vec(&decrypted).unwrap(),
            stderr: Vec::new(),
        }]);
        let remote = SopsRemote::new(&config, remote_config, &runner);
        let (secrets, version) = remote.pull(&config, "dev").unwrap().unwrap();

        assert_eq!(version, 5);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));
    }

    #[test]
    fn pull_missing_file_returns_none() {
        let config = make_config(sops_yaml());
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = SopsRemote::new(&config, remote_config, &runner);
        // Path "secrets/dev.enc.json" doesn't exist
        assert!(remote.pull(&config, "dev").unwrap().is_none());

        let calls = calls(&runner);
        assert!(calls.is_empty());
    }

    #[test]
    fn push_uses_env_version() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: myapp
environments: [dev]
remotes:
  sops:
    path: "{}/secrets/{{environment}}.enc.json"
"#,
            dir.path().display()
        );
        let config = make_config(&yaml);
        let remote_config: SopsRemoteConfig = config.remote_config("sops").unwrap();

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
                    stdout: b"encrypted".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }
        let runner = StdinCapture {
            calls: Mutex::new(Vec::new()),
        };
        let remote = SopsRemote::new(&config, remote_config, &runner);

        let mut env_versions = BTreeMap::new();
        env_versions.insert("dev".to_string(), 99);
        let payload = StorePayload {
            secrets: BTreeMap::from([("KEY:dev".to_string(), "val".to_string())]),
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions,
            env_last_changed_at: BTreeMap::new(),
        };
        remote.push(&payload, &config, "dev").unwrap();

        let calls = runner.calls.lock().unwrap();
        let stdin = calls[0].2.as_ref().unwrap();
        let json: BTreeMap<String, String> = serde_json::from_slice(stdin).unwrap();
        assert_eq!(json.get(crate::remotes::ESK_VERSION_KEY).unwrap(), "99");
    }
}
