//! Kubernetes target — deploys secrets as K8s Secret resources via `kubectl`.
//!
//! Kubernetes Secrets are objects that store sensitive data (passwords, tokens,
//! keys) separately from pod specs. They can be mounted as volumes or exposed
//! as environment variables to containers.
//!
//! CLI: `kubectl` (Kubernetes CLI).
//! Commands: `kubectl apply -f -` (via stdin).
//!
//! Operates in **batch mode**: generates a complete YAML `Secret` manifest with
//! all secrets base64-encoded in the `data` field, then applies it via stdin.
//! Secret and namespace names are validated against Kubernetes naming rules
//! (RFC 1123: lowercase alphanumeric and hyphens, max 253 chars). Supports
//! `--context` for multi-cluster setups.

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use crate::config::{Config, KubernetesTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, BatchDeployment, CommandOpts, CommandRunner, DeployMode,
    DeployOutcome, DeployResult, DeployTarget, SecretValue,
};
use crate::verify::{Evidence, Fidelity};

/// Validate a Kubernetes resource name or namespace.
///
/// Must match `[a-z0-9]([a-z0-9-]*[a-z0-9])?` and be at most 253 characters.
/// This prevents YAML injection via crafted names in the Secret manifest.
fn validate_k8s_name(name: &str, field: &str) -> Result<()> {
    if name.is_empty() {
        bail!("kubernetes {field} must not be empty");
    }
    if name.len() > 253 {
        let truncated: String = name.chars().take(32).collect();
        bail!("kubernetes {field} '{truncated}...' exceeds 253 character limit");
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

impl KubernetesTarget<'_> {
    fn resolve_namespace(&self, env: &str) -> Result<&str> {
        self.target_config
            .namespace
            .get(env)
            .map(std::string::String::as_str)
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
            let _ = writeln!(data_entries, "  {}: {}", s.key, encoded);
        }

        Ok(format!(
            "apiVersion: v1\nkind: Secret\nmetadata:\n  name: {name}\n  namespace: {ns}\ntype: Opaque\ndata:\n{data_entries}"
        ))
    }
}

impl DeployTarget for KubernetesTarget<'_> {
    fn name(&self) -> &'static str {
        "kubernetes"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Batch
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "kubectl")
            .context("Install from: https://kubernetes.io/docs/tasks/tools/")?;
        let output = self
            .runner
            .run("kubectl", &["cluster-info"], CommandOpts::default())
            .context("failed to run kubectl cluster-info")?;
        if !output.success {
            anyhow::bail!("kubectl cannot connect to a cluster. Run: kubectl config get-contexts");
        }
        Ok(())
    }

    fn deploy_secret(&self, _key: &str, _value: &str, _target: &ResolvedTarget) -> Result<()> {
        // Batch target — deploy_batch is the primary method
        Ok(())
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `kubectl get secret <name> -n <ns> -o json`.
    ///
    /// The `data` map is base64-encoded by the Kubernetes API, which is the
    /// same encoding [`Self::generate_manifest`] writes, so decoding here is
    /// the exact inverse of the deploy.
    ///
    /// `keys` is unused: one Secret object holds every key esk manages for
    /// this scope, and it arrives in a single request.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let ns = self.resolve_namespace(&target.environment)?;
        let name = self.secret_name();

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["get", "secret", &name, "-n", ns, "-o", "json"];
        if let Some(ctx) = self.target_config.context.get(&target.environment) {
            args.push("--context");
            args.push(ctx);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("kubectl", &args, CommandOpts::default())
            .context("failed to run kubectl get secret")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // A deleted Secret is the state this check exists to catch, so it
            // is reported as an empty read — every managed key comes back
            // missing — rather than as an unreadable cluster.
            if stderr.contains("NotFound") || stderr.contains("not found") {
                return Ok(Evidence::Values(BTreeMap::new()));
            }
            anyhow::bail!("kubectl get secret failed for {name}: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse kubectl get secret JSON response")?;

        // An existing Secret with no `data` holds no keys, which is a valid
        // empty read rather than a parse failure.
        let Some(data) = json.get("data").and_then(|d| d.as_object()) else {
            return Ok(Evidence::Values(BTreeMap::new()));
        };

        let mut values = BTreeMap::new();
        for (key, encoded) in data {
            let encoded = encoded
                .as_str()
                .with_context(|| format!("secret data for '{key}' was not a string"))?;
            let decoded = BASE64
                .decode(encoded)
                .with_context(|| format!("secret data for '{key}' was not valid base64"))?;
            // Lossy decode: a value written outside esk may not be UTF-8, and
            // failing the whole read would hide the keys that are fine. A
            // mangled value still mismatches, which is the honest outcome.
            values.insert(
                key.clone(),
                Zeroizing::new(String::from_utf8_lossy(&decoded).into_owned()),
            );
        }

        Ok(Evidence::Values(values))
    }

    fn deploy_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<DeployResult> {
        self.deploy_batch_state(BatchDeployment::without_tombstones(secrets), target)
            .unwrap_or_else(|error| {
                secrets
                    .iter()
                    .map(|secret| DeployResult {
                        key: secret.key.clone(),
                        outcome: DeployOutcome::Failed(error.to_string()),
                    })
                    .collect()
            })
    }

    fn deploy_batch_state(
        &self,
        batch: BatchDeployment<'_>,
        target: &ResolvedTarget,
    ) -> Result<Vec<DeployResult>> {
        let manifest = self.generate_manifest(batch.secrets, target)?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["apply", "-f", "-"];

        if let Some(ctx) = self.target_config.context.get(&target.environment) {
            args.push("--context");
            // We need to hold the string alive
            // Push context separately to keep lifetime
            args.push(ctx);
        }

        args.extend(flag_parts.iter().map(String::as_str));

        let result = self.runner.run(
            "kubectl",
            &args,
            CommandOpts {
                stdin: Some(manifest.into_bytes()),
                ..Default::default()
            },
        );

        let output = result?;
        if output.success {
            Ok(batch
                .secrets
                .iter()
                .map(|s| DeployResult {
                    key: s.key.clone(),
                    outcome: DeployOutcome::Success,
                })
                .collect())
        } else {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_config() -> ConfigFixture {
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
        ConfigFixture::new(yaml).expect("fixture")
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
            value: zeroize::Zeroizing::new(value.to_string()),
            group: "G".to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
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
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["cluster-info"]);
    }

    #[test]
    fn preflight_cluster_unreachable() {
        let fixture = make_config();
        let config = fixture.config();
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
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("cannot connect to a cluster"));
    }

    #[test]
    fn preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Install from: https://kubernetes.io"));
    }

    #[test]
    fn deploy_batch_generates_manifest() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![
            make_secret("DB_HOST", "localhost"),
            make_secret("DB_PASS", "s3cret"),
        ];
        let results = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target("dev"),
            )
            .unwrap();
        assert!(results.iter().all(|r| r.outcome.is_success()));

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "kubectl");
        assert_eq!(calls[0].args, vec!["apply", "-f", "-"]);

        // Verify manifest content
        let stdin = String::from_utf8(calls[0].stdin.clone().unwrap()).unwrap();
        assert!(stdin.contains("kind: Secret"));
        assert!(stdin.contains("namespace: myapp-dev"));
        assert!(stdin.contains("name: myapp-secrets"));
        // Check base64 encoding
        assert!(stdin.contains(&BASE64.encode(b"localhost")));
        assert!(stdin.contains(&BASE64.encode(b"s3cret")));
    }

    #[test]
    fn deploy_batch_with_context_and_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target("prod"),
            )
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--context".to_string()));
        assert!(calls[0].args.contains(&"prod-cluster".to_string()));
        assert!(calls[0].args.contains(&"--dry-run=client".to_string()));
    }

    #[test]
    fn deploy_batch_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"forbidden".to_vec(),
        }]);
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let error = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target("dev"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("forbidden"));
    }

    #[test]
    fn deploy_batch_unknown_namespace() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let error = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target("staging"),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("no kubernetes namespace mapping"));
    }

    #[test]
    fn deploy_batch_too_long_namespace_errors_cleanly() {
        // A too-long namespace from config must surface as a clean deploy
        // error, not a panic, even when it contains a multi-byte char.
        let dir = tempfile::tempdir().unwrap();
        let mut long_ns = "a".repeat(31);
        long_ns.push('é');
        long_ns.push_str(&"a".repeat(230));
        assert!(long_ns.len() > 253);
        let yaml = format!(
            "project: myapp\nenvironments: [dev]\ntargets:\n  kubernetes:\n    namespace:\n      dev: {long_ns}\n"
        );
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = KubernetesTarget {
            config: &config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let error = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target("dev"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("exceeds 253"));
    }

    #[test]
    fn default_secret_name() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &MockCommandRunner::from_outputs(vec![]),
        };
        assert_eq!(target.secret_name(), "myapp-secrets");
    }

    #[test]
    fn custom_secret_name() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
targets:
  kubernetes:
    namespace:
      dev: ns
    secret_name: custom-secret
";
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
    fn validate_k8s_name_too_long_multibyte_straddling_boundary_does_not_panic() {
        // 31 ASCII chars, then 'é' (2 bytes) straddles byte offset 32
        // (bytes 31..33), then filler past the 253-byte limit. A naive
        // `&name[..32]` byte slice panics with "not a char boundary";
        // char-based truncation must not.
        let mut name = "a".repeat(31);
        name.push('é');
        name.push_str(&"a".repeat(230));
        assert!(name.len() > 253);
        let err = validate_k8s_name(&name, "name").unwrap_err();
        assert!(err.to_string().contains("exceeds 253"));
    }

    #[test]
    fn validate_k8s_name_too_long_emoji_at_different_offset_does_not_panic() {
        // Wider (4-byte) multi-byte char at a different straddling offset.
        let mut name = "a".repeat(30);
        name.push('🚀');
        name.push_str(&"a".repeat(230));
        assert!(name.len() > 253);
        let err = validate_k8s_name(&name, "name").unwrap_err();
        assert!(err.to_string().contains("exceeds 253"));
    }

    #[test]
    fn deploy_batch_empty() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };
        let results = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[]),
                &make_target("dev"),
            )
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn deploy_batch_empty_propagates_apply_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"cannot replace final-secret manifest".to_vec(),
        }]);
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let error = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[]),
                &make_target("dev"),
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot replace final-secret manifest"));
    }

    fn verify_keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn verify_expected(pairs: &[(&str, &str)]) -> BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    /// A `kubectl get secret -o json` response with base64-encoded data.
    fn secret_json(pairs: &[(&str, &str)]) -> Vec<u8> {
        let data: serde_json::Map<String, serde_json::Value> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::json!(BASE64.encode(v))))
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "data": data,
        }))
        .unwrap()
    }

    #[test]
    fn kubernetes_read_back_decodes_base64_data() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_json(&[("API_KEY", "secret1")]), b"");
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target("dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("kubernetes declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "kubectl");
        assert_eq!(
            calls[0].args,
            vec![
                "get",
                "secret",
                "myapp-secrets",
                "-n",
                "myapp-dev",
                "-o",
                "json"
            ]
        );
    }

    /// The round trip that matters: whatever `generate_manifest` encodes, the
    /// reader must decode identically, or every deploy reports false drift.
    #[test]
    fn kubernetes_read_back_round_trips_the_manifest_encoding() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let awkward = [
            ("PLAIN", "simple"),
            ("SPACES", "hello world"),
            ("NEWLINES", "line1\nline2"),
            ("UNICODE", "café ☕"),
            ("EQUALS", "a=b=c"),
            ("EMPTY", ""),
        ];
        let secrets: Vec<SecretValue> = awkward
            .iter()
            .map(|(k, v)| SecretValue {
                key: (*k).to_string(),
                value: Zeroizing::new((*v).to_string()),
                group: "G".to_string(),
            })
            .collect();

        // Encode exactly as a deploy would, then feed it back as the cluster's
        // response so the two halves are checked against each other.
        let manifest = target
            .generate_manifest(&secrets, &make_target("dev"))
            .unwrap();
        let encoded: Vec<(String, String)> = manifest
            .lines()
            .skip_while(|l| !l.starts_with("data:"))
            .skip(1)
            // Split on the colon, not on ": ": an empty value encodes to an
            // empty string, so its line ends with the separator and no value.
            .filter_map(|l| l.trim().split_once(':'))
            .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
            .collect();
        let data: serde_json::Map<String, serde_json::Value> = encoded
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::json!(v)))
            .collect();
        runner.push_success(
            &serde_json::to_vec(&serde_json::json!({ "data": data })).unwrap(),
            b"",
        );

        let evidence = target
            .read_back(&verify_keys(&["PLAIN"]), &make_target("dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values");
        };
        for (key, value) in awkward {
            assert_eq!(
                values.get(key).map(|v| v.as_str()),
                Some(value),
                "{key} did not survive a manifest write/read round trip"
            );
        }
    }

    #[test]
    fn kubernetes_read_back_surfaces_wrong_value_as_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_json(&[("API_KEY", "STALE")]), b"");
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "current")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Differs);
    }

    /// A deleted Secret is exactly the drift this command exists to catch, so
    /// it must name the missing keys rather than report an unreadable cluster.
    #[test]
    fn kubernetes_read_back_deleted_secret_reports_missing_keys() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"Error from server (NotFound): secrets \"myapp-secrets\" not found");
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Missing);
    }

    /// Any other failure is an unreadable cluster, not an absent Secret.
    /// Collapsing the two would report drift on a scope esk never read.
    #[test]
    fn kubernetes_read_back_other_failure_is_unreachable() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"Unable to connect to the server: dial tcp: i/o timeout");
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
        assert!(matches!(
            findings,
            crate::verify::Findings::Unreachable { .. }
        ));
    }

    #[test]
    fn kubernetes_read_back_applies_context_and_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.kubernetes.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_json(&[("A", "1")]), b"");
        let target = KubernetesTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .read_back(&verify_keys(&["A"]), &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--context".to_string()));
        assert!(calls[0].args.contains(&"prod-cluster".to_string()));
        assert!(calls[0].args.contains(&"myapp-prod".to_string()));
    }
}
