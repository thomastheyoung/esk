use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

use crate::targets::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SecretValue,
    DeployTarget, DeployMode, DeployResult,
};
use crate::config::{Config, KubernetesTargetConfig, ResolvedTarget};

/// Validate a Kubernetes resource name or namespace.
///
/// Must match `[a-z0-9]([a-z0-9-]*[a-z0-9])?` and be at most 253 characters.
/// This prevents YAML injection via crafted names in the Secret manifest.
fn validate_k8s_name(name: &str, field: &str) -> Result<()> {
    if name.is_empty() {
        bail!("kubernetes {field} must not be empty");
    }
    if name.len() > 253 {
        bail!(
            "kubernetes {field} '{}...' exceeds 253 character limit",
            &name[..32]
        );
    }
    let bytes = name.as_bytes();
    // First char must be [a-z0-9]
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        bail!("kubernetes {field} '{name}' must start with a lowercase letter or digit");
    }
    // Last char must be [a-z0-9]
    let last = bytes[bytes.len() - 1];
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        bail!("kubernetes {field} '{name}' must end with a lowercase letter or digit");
    }
    // Middle chars must be [a-z0-9-]
    if bytes.len() > 2 {
        for &b in &bytes[1..bytes.len() - 1] {
            if !b.is_ascii_lowercase() && !b.is_ascii_digit() && b != b'-' {
                bail!("kubernetes {field} '{name}' contains invalid character '{}'; only lowercase letters, digits, and hyphens are allowed", b as char);
            }
        }
    }
    Ok(())
}

pub struct KubernetesTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a KubernetesTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> KubernetesTarget<'a> {
    fn resolve_namespace(&self, env: &str) -> Result<&str> {
        self.target_config
            .namespace
            .get(env)
            .map(|s| s.as_str())
            .with_context(|| format!("no kubernetes namespace mapping for '{env}'"))
    }

    fn secret_name(&self) -> String {
        self.target_config
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

        validate_k8s_name(&name, "secret name")?;
        validate_k8s_name(ns, "namespace")?;

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

impl<'a> DeployTarget for KubernetesTarget<'a> {
    fn name(&self) -> &str {
        "kubernetes"
    }

    fn sync_mode(&self) -> DeployMode {
        DeployMode::Batch
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
        // Batch target — sync_batch is the primary method
        Ok(())
    }

    fn sync_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<DeployResult> {
        let manifest = match self.generate_manifest(secrets, target) {
            Ok(m) => m,
            Err(e) => {
                return secrets
                    .iter()
                    .map(|s| DeployResult {
                        key: s.key.clone(),
                        target: target.clone(),
                        success: false,
                        error: Some(e.to_string()),
                    })
                    .collect();
            }
        };

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["apply", "-f", "-"];

        if let Some(ctx) = self.target_config.context.get(&target.environment) {
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
                .map(|s| DeployResult {
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
                    .map(|s| DeployResult {
                        key: s.key.clone(),
                        target: target.clone(),
                        success: false,
                        error: Some(stderr.clone()),
                    })
                    .collect()
            }
            Err(e) => secrets
                .iter()
                .map(|s| DeployResult {
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
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (String, Vec<String>, Option<Vec<u8>>);

    fn take_calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .take_calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.stdin))
            .collect()
    }

    fn make_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: myapp
environments: [dev, prod]
targets:
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
            service: "kubernetes".to_string(),
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
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
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
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["cluster-info"]);
    }

    #[test]
    fn preflight_cluster_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
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
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("cannot connect to a cluster"));
    }

    #[test]
    fn preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("kubectl is not installed"));
    }

    #[test]
    fn sync_batch_generates_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![
            make_secret("DB_HOST", "localhost"),
            make_secret("DB_PASS", "s3cret"),
        ];
        let results = target.sync_batch(&secrets, &make_target("dev"));
        assert!(results.iter().all(|r| r.success));

        let calls = take_calls(&runner);
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
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        target.sync_batch(&secrets, &make_target("prod"));

        let calls = take_calls(&runner);
        assert!(calls[0].1.contains(&"--context".to_string()));
        assert!(calls[0].1.contains(&"prod-cluster".to_string()));
        assert!(calls[0].1.contains(&"--dry-run=client".to_string()));
    }

    #[test]
    fn sync_batch_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"forbidden".to_vec(),
        }]);
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let results = target.sync_batch(&secrets, &make_target("dev"));
        assert!(!results[0].success);
        assert!(results[0].error.as_ref().unwrap().contains("forbidden"));
    }

    #[test]
    fn sync_batch_unknown_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let results = target.sync_batch(&secrets, &make_target("staging"));
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
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &MockCommandRunner::from_outputs(vec![]),
        };
        assert_eq!(target.secret_name(), "myapp-secrets");
    }

    #[test]
    fn custom_secret_name() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
targets:
  kubernetes:
    namespace:
      dev: ns
    secret_name: custom-secret
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &MockCommandRunner::from_outputs(vec![]),
        };
        assert_eq!(target.secret_name(), "custom-secret");
    }

    #[test]
    fn validate_k8s_name_valid() {
        assert!(validate_k8s_name("myapp-secrets", "name").is_ok());
        assert!(validate_k8s_name("a", "name").is_ok());
        assert!(validate_k8s_name("abc123", "name").is_ok());
        assert!(validate_k8s_name("my-ns", "namespace").is_ok());
    }

    #[test]
    fn validate_k8s_name_uppercase_fails() {
        let err = validate_k8s_name("MyApp", "name").unwrap_err();
        assert!(err.to_string().contains("must start with a lowercase"));
    }

    #[test]
    fn validate_k8s_name_newline_fails() {
        let err = validate_k8s_name("my\nname", "name").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn validate_k8s_name_leading_hyphen_fails() {
        let err = validate_k8s_name("-myname", "name").unwrap_err();
        assert!(err.to_string().contains("must start with"));
    }

    #[test]
    fn validate_k8s_name_empty_fails() {
        let err = validate_k8s_name("", "name").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn validate_k8s_name_too_long_fails() {
        let long_name = "a".repeat(254);
        let err = validate_k8s_name(&long_name, "name").unwrap_err();
        assert!(err.to_string().contains("exceeds 253"));
    }

    #[test]
    fn sync_batch_empty() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let results = target.sync_batch(&[], &make_target("dev"));
        assert!(results.is_empty());
    }
}
