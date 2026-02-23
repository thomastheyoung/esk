use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SecretValue,
    SyncAdapter, SyncMode, SyncResult,
};
use crate::config::{Config, KubernetesAdapterConfig, ResolvedTarget};

pub struct KubernetesAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a KubernetesAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> KubernetesAdapter<'a> {
    fn resolve_namespace(&self, env: &str) -> Result<&str> {
        self.adapter_config
            .namespace
            .get(env)
            .map(|s| s.as_str())
            .with_context(|| format!("no kubernetes namespace mapping for '{env}'"))
    }

    fn secret_name(&self) -> String {
        self.adapter_config
            .secret_name
            .clone()
            .unwrap_or_else(|| format!("{}-secrets", self.config.project))
    }

    fn generate_manifest(
        &self,
        secrets: &[SecretValue],
        target: &ResolvedTarget,
    ) -> Result<String> {
        let ns = self.resolve_namespace(&target.environment)?;
        let name = self.secret_name();

        let mut data_entries = String::new();
        for s in secrets {
            let encoded = BASE64.encode(s.value.as_bytes());
            data_entries.push_str(&format!("  {}: {}\n", s.key, encoded));
        }

        Ok(format!(
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {name}\n  namespace: {ns}\ntype: Opaque\ndata:\n{data_entries}"
        ))
    }
}

impl<'a> SyncAdapter for KubernetesAdapter<'a> {
    fn name(&self) -> &str {
        "kubernetes"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Batch
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "kubectl").map_err(|_| {
            anyhow::anyhow!(
                "kubectl is not installed or not in PATH. Install it from: https://kubernetes.io/docs/tasks/tools/"
            )
        })?;

        let output = self
            .runner
            .run("kubectl", &["cluster-info"], CommandOpts::default())
            .context("failed to run kubectl cluster-info")?;
        if !output.success {
            anyhow::bail!(
                "kubectl cannot connect to a cluster. Check your kubeconfig and cluster status."
            );
        }

        Ok(())
    }

    fn sync_secret(&self, _key: &str, _value: &str, _target: &ResolvedTarget) -> Result<()> {
        // Batch adapter — sync_batch is the primary method
        Ok(())
    }

    fn sync_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<SyncResult> {
        let manifest = match self.generate_manifest(secrets, target) {
            Ok(m) => m,
            Err(e) => {
                return secrets
                    .iter()
                    .map(|s| SyncResult {
                        key: s.key.clone(),
                        target: target.clone(),
                        success: false,
                        error: Some(e.to_string()),
                    })
                    .collect();
            }
        };

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["apply", "-f", "-"];

        if let Some(ctx) = self.adapter_config.context.get(&target.environment) {
            args.push("--context");
            // We need to hold the string alive
            // Push context separately to keep lifetime
            args.push(ctx);
        }

        append_env_flags(&mut args, &flag_parts);

        let result = self.runner.run(
            "kubectl",
            &args,
            CommandOpts {
                stdin: Some(manifest.into_bytes()),
                ..Default::default()
            },
        );

        match result {
            Ok(output) if output.success => secrets
                .iter()
                .map(|s| SyncResult {
                    key: s.key.clone(),
                    target: target.clone(),
                    success: true,
                    error: None,
                })
                .collect(),
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                secrets
                    .iter()
                    .map(|s| SyncResult {
                        key: s.key.clone(),
                        target: target.clone(),
                        success: false,
                        error: Some(stderr.clone()),
                    })
                    .collect()
            }
            Err(e) => secrets
                .iter()
                .map(|s| SyncResult {
                    key: s.key.clone(),
                    target: target.clone(),
                    success: false,
                    error: Some(e.to_string()),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{CommandOpts, CommandOutput, CommandRunner};
    use std::sync::Mutex;

    struct MockRunner {
        calls: Mutex<Vec<(String, Vec<String>, Option<Vec<u8>>)>>,
        responses: Mutex<Vec<CommandOutput>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }
        fn take_calls(&self) -> Vec<(String, Vec<String>, Option<Vec<u8>>)> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl CommandRunner for MockRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            opts: CommandOpts,
        ) -> anyhow::Result<CommandOutput> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
                opts.stdin,
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

    fn make_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: myapp
environments: [dev, prod]
adapters:
  kubernetes:
    namespace:
      dev: myapp-dev
      prod: myapp-prod
    context:
      prod: prod-cluster
    env_flags:
      prod: "--dry-run=client"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            adapter: "kubernetes".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    fn make_secret(key: &str, value: &str) -> SecretValue {
        SecretValue {
            key: key.to_string(),
            value: value.to_string(),
            vendor: "G".to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let runner = MockRunner::new(vec![
            CommandOutput {
                success: true,
                stdout: b"1.28.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"Kubernetes control plane is running".to_vec(),
                stderr: vec![],
            },
        ]);
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["cluster-info"]);
    }

    #[test]
    fn preflight_cluster_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let runner = MockRunner::new(vec![
            CommandOutput {
                success: true,
                stdout: b"1.28.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"connection refused".to_vec(),
            },
        ]);
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("cannot connect to a cluster"));
    }

    #[test]
    fn preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &FailRunner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("kubectl is not installed"));
    }

    #[test]
    fn sync_batch_generates_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };

        let secrets = vec![
            make_secret("DB_HOST", "localhost"),
            make_secret("DB_PASS", "s3cret"),
        ];
        let results = adapter.sync_batch(&secrets, &make_target("dev"));
        assert!(results.iter().all(|r| r.success));

        let calls = runner.take_calls();
        assert_eq!(calls[0].0, "kubectl");
        assert_eq!(calls[0].1, vec!["apply", "-f", "-"]);

        // Verify manifest content
        let stdin = String::from_utf8(calls[0].2.clone().unwrap()).unwrap();
        assert!(stdin.contains("kind: Secret"));
        assert!(stdin.contains("namespace: myapp-dev"));
        assert!(stdin.contains("name: myapp-secrets"));
        // Check base64 encoding
        assert!(stdin.contains(&BASE64.encode(b"localhost")));
        assert!(stdin.contains(&BASE64.encode(b"s3cret")));
    }

    #[test]
    fn sync_batch_with_context_and_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        adapter.sync_batch(&secrets, &make_target("prod"));

        let calls = runner.take_calls();
        assert!(calls[0].1.contains(&"--context".to_string()));
        assert!(calls[0].1.contains(&"prod-cluster".to_string()));
        assert!(calls[0].1.contains(&"--dry-run=client".to_string()));
    }

    #[test]
    fn sync_batch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"forbidden".to_vec(),
        }]);
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let results = adapter.sync_batch(&secrets, &make_target("dev"));
        assert!(!results[0].success);
        assert!(results[0].error.as_ref().unwrap().contains("forbidden"));
    }

    #[test]
    fn sync_batch_unknown_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let runner = MockRunner::new(vec![]);
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let results = adapter.sync_batch(&secrets, &make_target("staging"));
        assert!(!results[0].success);
        assert!(results[0]
            .error
            .as_ref()
            .unwrap()
            .contains("no kubernetes namespace mapping"));
    }

    #[test]
    fn default_secret_name() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &MockRunner::new(vec![]),
        };
        assert_eq!(adapter.secret_name(), "myapp-secrets");
    }

    #[test]
    fn custom_secret_name() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
adapters:
  kubernetes:
    namespace:
      dev: ns
    secret_name: custom-secret
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &MockRunner::new(vec![]),
        };
        assert_eq!(adapter.secret_name(), "custom-secret");
    }

    #[test]
    fn sync_batch_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.kubernetes.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = KubernetesAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let results = adapter.sync_batch(&[], &make_target("dev"));
        assert!(results.is_empty());
    }
}
