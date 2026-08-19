//! GCP Cloud Run target — deploys environment variables via the `gcloud` CLI.
//!
//! Cloud Run is Google's serverless container platform. Environment variables
//! are set per service via `gcloud run services update --update-env-vars KEY=VALUE`.
//!
//! CLI: `gcloud` (Google Cloud CLI).
//! Commands: `gcloud run services update --update-env-vars` / `--remove-env-vars`.
//!
//! The gcloud CLI does **not** support stdin for updating env vars, so values
//! are passed as `--update-env-vars KEY=VALUE` command-line arguments (visible
//! in `ps` output). Requires a service name (mapped from esk's app config),
//! a GCP project, and a region.

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, GcpCloudRunTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct GcpCloudRunTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a GcpCloudRunTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl GcpCloudRunTarget<'_> {
    fn resolve_service(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("gcp_cloud_run target requires an app")?;
        self.target_config
            .service_names
            .get(app)
            .map(std::string::String::as_str)
            .with_context(|| format!("no gcp_cloud_run service_names mapping for '{app}'"))
    }
}

impl DeployTarget for GcpCloudRunTarget<'_> {
    fn name(&self) -> &'static str {
        "gcp_cloud_run"
    }

    fn passes_value_as_cli_arg(&self) -> bool {
        true
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "gcloud")
            .context("Install from: https://cloud.google.com/sdk/docs/install")?;
        let project = &self.target_config.project;
        let output = self
            .runner
            .run(
                "gcloud",
                &["auth", "print-access-token", "--project", project],
                CommandOpts::default(),
            )
            .context("failed to run gcloud auth print-access-token")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "GCP project '{project}' not accessible. Run: gcloud auth login\n{stderr}"
            );
        }
        Ok(())
    }

    // SECURITY: gcloud run services update has no stdin support for env vars. Secret values are
    // exposed in process arguments (visible via `ps aux`). No workaround available.
    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let service = self.resolve_service(target)?;
        let project = &self.target_config.project;
        let region = &self.target_config.region;
        let kv = format!("{key}={value}");

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "run",
            "services",
            "update",
            service,
            "--update-env-vars",
            &kv,
            "--project",
            project,
            "--region",
            region,
        ];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("gcloud", &args, CommandOpts::default())
            .with_context(|| format!("failed to run gcloud run services update for {key}"))?;

        output.check("gcloud run services update", key)
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `gcloud run services describe --format=json`.
    ///
    /// Cloud Run's `EnvVar` carries either a literal `value` or a
    /// `valueSource` pointing at Secret Manager. Only literals are read back.
    ///
    /// A `valueSource` where esk deployed a literal is not a gap in esk's
    /// knowledge — it is the drift. esk writes with `--update-env-vars`, which
    /// sets a literal, so a reference standing in its place means something
    /// replaced what esk deployed, and the service is no longer serving the
    /// store's value. Reporting the key `Missing` is therefore a true claim.
    ///
    /// This holds because esk deploys with `--update-env-vars`, which always
    /// writes a literal. A user whose `env_flags` add `--set-secrets` is
    /// deploying references themselves, and those keys will read as missing.
    ///
    /// This is the opposite call from `gitlab`, deliberately. There a hidden
    /// variable's value is withheld by the provider while the key may be
    /// perfectly correct, so claiming anything about it would be a falsehood;
    /// that target reports presence instead. Here the provider is telling esk
    /// something real about the deployment's shape.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let service = self.resolve_service(target)?;
        let project = &self.target_config.project;
        let region = &self.target_config.region;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "run",
            "services",
            "describe",
            service,
            "--project",
            project,
            "--region",
            region,
            "--format",
            "json",
        ];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("gcloud", &args, CommandOpts::default())
            .context("failed to run gcloud run services describe")?;

        // A failed describe is an incomplete read, never an empty service.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gcloud run services describe failed for {service}: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse gcloud run services describe JSON response")?;

        let containers = json
            .pointer("/spec/template/spec/containers")
            .and_then(|c| c.as_array())
            .context("gcloud response had no spec.template.spec.containers")?;

        let mut values: BTreeMap<String, Zeroizing<String>> = BTreeMap::new();
        for container in containers {
            let Some(env) = container.get("env").and_then(|e| e.as_array()) else {
                continue;
            };
            for entry in env {
                let (Some(name), Some(value)) = (
                    entry.get("name").and_then(|n| n.as_str()),
                    // `value` only. A `valueSource` entry is deliberately
                    // skipped rather than stringified.
                    entry.get("value").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                // Containers can disagree — a sidecar may declare the same
                // name with a different value. esk cannot say which one the
                // key "really" holds, and picking one silently would assert a
                // verdict it has no basis for, so an ambiguous read is
                // reported as an incomplete one.
                if let Some(existing) = values.get(name) {
                    if existing.as_str() != value {
                        anyhow::bail!(
                            "containers in {service} disagree on '{name}'; esk cannot \
                             determine which value the service holds"
                        );
                    }
                }
                values.insert(name.to_string(), Zeroizing::new(value.to_string()));
            }
        }

        Ok(Evidence::Values(values))
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let service = self.resolve_service(target)?;
        let project = &self.target_config.project;
        let region = &self.target_config.region;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "run",
            "services",
            "update",
            service,
            "--remove-env-vars",
            key,
            "--project",
            project,
            "--region",
            region,
        ];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("gcloud", &args, CommandOpts::default())
            .with_context(|| {
                format!("failed to run gcloud run services update --remove-env-vars for {key}")
            })?;

        output.check("gcloud run services update --remove-env-vars", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_config() -> ConfigFixture {
        let yaml = r#"
project: x
environments: [dev, staging, prod]
apps:
  web:
    path: apps/web
  api:
    path: apps/api
targets:
  gcp_cloud_run:
    service_names:
      web: my-web-service
      api: my-api-service
    project: my-gcp-project
    region: us-central1
    env_flags:
      prod: "--project my-prod-project --region europe-west1"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "gcp_cloud_run".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"Google Cloud SDK 400.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"ya29.token".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(
            calls[1].args,
            vec!["auth", "print-access-token", "--project", "my-gcp-project"]
        );
    }

    #[test]
    fn preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"Google Cloud SDK 400.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"ERROR: not authenticated".to_vec(),
            },
        ]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("not accessible"));
    }

    #[test]
    fn preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Install from: https://cloud.google.com"));
    }

    #[test]
    fn deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "gcloud");
        assert_eq!(
            calls[0].args,
            vec![
                "run",
                "services",
                "update",
                "my-web-service",
                "--update-env-vars",
                "MY_KEY=secret_val",
                "--project",
                "my-gcp-project",
                "--region",
                "us-central1",
            ]
        );
    }

    #[test]
    fn deploy_different_app() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "val", &make_target(Some("api"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"my-api-service".to_string()));
    }

    #[test]
    fn deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--project".to_string()));
        assert!(calls[0].args.contains(&"my-prod-project".to_string()));
        assert!(calls[0].args.contains(&"--region".to_string()));
        assert!(calls[0].args.contains(&"europe-west1".to_string()));
    }

    #[test]
    fn delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "run",
                "services",
                "update",
                "my-web-service",
                "--remove-env-vars",
                "MY_KEY",
                "--project",
                "my-gcp-project",
                "--region",
                "us-central1",
            ]
        );
    }

    #[test]
    fn requires_app() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("requires an app"));
    }

    #[test]
    fn unknown_app_mapping() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("unknown"), "dev"))
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("no gcp_cloud_run service_names mapping"));
    }

    #[test]
    fn nonzero_exit() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"permission denied".to_vec(),
        }]);
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("permission denied"));
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

    /// A `describe` response whose container env holds literal values.
    fn describe_json(env: &[serde_json::Value]) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "spec": { "template": { "spec": { "containers": [ { "env": env } ] } } }
        }))
        .unwrap()
    }

    #[test]
    fn gcp_read_back_returns_literal_env_values() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &describe_json(&[serde_json::json!({ "name": "API_KEY", "value": "secret1" })]),
            b"",
        );
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("gcp declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "gcloud");
        assert!(calls[0].args.contains(&"describe".to_string()));
        assert!(calls[0].args.contains(&"my-web-service".to_string()));
    }

    /// A Secret Manager reference standing where esk deployed a literal is
    /// real drift, not a blind spot: something replaced esk's env var, and the
    /// service is no longer serving the store's value. Reporting the key
    /// `Missing` is a true claim, unlike the `gitlab` hidden-variable case
    /// where the provider withholds a value that may well be correct.
    #[test]
    fn gcp_read_back_skips_secret_manager_references() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &describe_json(&[
                serde_json::json!({ "name": "LITERAL", "value": "v" }),
                serde_json::json!({
                    "name": "FROM_SECRET_MANAGER",
                    "valueSource": {
                        "secretKeyRef": { "secret": "my-secret", "version": "1" }
                    }
                }),
            ]),
            b"",
        );
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(
                &verify_keys(&["LITERAL", "FROM_SECRET_MANAGER"]),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values");
        };
        assert_eq!(values["LITERAL"].as_str(), "v");
        assert!(
            !values.contains_key("FROM_SECRET_MANAGER"),
            "a secretKeyRef must never be compared as if it were the value"
        );

        // And it surfaces as drift on that key, which is the honest report:
        // esk deployed a literal and the service no longer holds one.
        let findings = crate::verify::compare(
            Fidelity::Value,
            Ok(Evidence::Values(values)),
            &verify_expected(&[("LITERAL", "v"), ("FROM_SECRET_MANAGER", "esk_literal")]),
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["LITERAL"], crate::verify::ValueVerdict::Matches);
        assert_eq!(
            verdicts["FROM_SECRET_MANAGER"],
            crate::verify::ValueVerdict::Missing
        );
    }

    #[test]
    fn gcp_read_back_surfaces_wrong_value_as_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &describe_json(&[serde_json::json!({ "name": "API_KEY", "value": "STALE" })]),
            b"",
        );
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "current")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
    }

    #[test]
    fn gcp_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"NOT_FOUND: Service not found");
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
    }

    /// Two containers disagreeing on a key is a state esk cannot resolve, so
    /// it reports an incomplete read rather than silently taking one of them.
    #[test]
    fn gcp_read_back_disagreeing_containers_is_an_incomplete_read() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &serde_json::to_vec(&serde_json::json!({
                "spec": { "template": { "spec": { "containers": [
                    { "env": [ { "name": "API_KEY", "value": "correct" } ] },
                    { "env": [ { "name": "API_KEY", "value": "sidecar-stale" } ] }
                ] } } }
            }))
            .unwrap(),
            b"",
        );
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "correct")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Unresolved,
            "an ambiguous read must not produce a value verdict"
        );
    }

    /// Agreeing containers are not a conflict — a sidecar legitimately sharing
    /// the same value must still verify.
    #[test]
    fn gcp_read_back_agreeing_containers_are_fine() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gcp_cloud_run.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &serde_json::to_vec(&serde_json::json!({
                "spec": { "template": { "spec": { "containers": [
                    { "env": [ { "name": "API_KEY", "value": "same" } ] },
                    { "env": [ { "name": "API_KEY", "value": "same" } ] }
                ] } } }
            }))
            .unwrap(),
            b"",
        );
        let target = GcpCloudRunTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "same")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: false }
        );
    }
}
