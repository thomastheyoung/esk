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

use crate::config::{Config, GcpCloudRunTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

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
        check_command(self.runner, "gcloud").map_err(|_| {
            anyhow::anyhow!(
                "Google Cloud CLI (gcloud) is not installed or not in PATH. Install it from: https://cloud.google.com/sdk/docs/install"
            )
        })?;
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
        assert!(err.to_string().contains("gcloud"));
        assert!(err.to_string().contains("not installed"));
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
}
