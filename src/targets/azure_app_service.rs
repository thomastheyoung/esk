//! Azure App Service target — deploys app settings via the `az` CLI.
//!
//! Azure App Service is Microsoft's PaaS for hosting web apps, REST APIs,
//! and mobile backends. App settings are exposed as environment variables
//! to the running application.
//!
//! CLI: `az` (Azure CLI).
//! Commands: `az webapp config appsettings set` / `az webapp config appsettings delete`.
//!
//! The Azure CLI does **not** reliably support stdin for setting values, so
//! they are passed as `--settings KEY=VALUE` command-line arguments (visible
//! in `ps` output). Requires an app name (mapped from esk's app config) and
//! a resource group.

use anyhow::{Context, Result};

use crate::config::{AzureAppServiceTargetConfig, Config, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct AzureAppServiceTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a AzureAppServiceTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl AzureAppServiceTarget<'_> {
    fn resolve_app(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("azure_app_service target requires an app")?;
        self.target_config
            .app_names
            .get(app)
            .map(std::string::String::as_str)
            .with_context(|| format!("no azure_app_service app_names mapping for '{app}'"))
    }

    fn resolve_slot(&self, env: &str) -> Option<&str> {
        self.target_config.slot.get(env).map(String::as_str)
    }

    fn base_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(sub) = &self.target_config.subscription {
            args.push("--subscription".to_string());
            args.push(sub.clone());
        }
        args
    }
}

impl DeployTarget for AzureAppServiceTarget<'_> {
    fn name(&self) -> &'static str {
        "azure_app_service"
    }

    fn passes_value_as_cli_arg(&self) -> bool {
        true
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "az").map_err(|_| {
            anyhow::anyhow!(
                "az is not installed or not in PATH. Install it from: https://learn.microsoft.com/en-us/cli/azure/install-azure-cli"
            )
        })?;
        let base = self.base_args();
        let mut args: Vec<&str> = vec!["account", "show"];
        args.extend(base.iter().map(String::as_str));
        let output = self
            .runner
            .run("az", &args, CommandOpts::default())
            .context("failed to run az account show")?;
        if !output.success {
            anyhow::bail!("az is not authenticated. Run: az login");
        }
        Ok(())
    }

    // SECURITY: az CLI has no reliable stdin support for appsettings set. Secret values are
    // exposed in process arguments (visible via `ps aux`). No workaround available.
    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let azure_app = self.resolve_app(target)?;
        let rg = &self.target_config.resource_group;
        let kv = format!("{key}={value}");
        let base = self.base_args();

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "webapp",
            "config",
            "appsettings",
            "set",
            "--name",
            azure_app,
            "--resource-group",
            rg,
            "--settings",
            &kv,
        ];
        if let Some(slot) = self.resolve_slot(&target.environment) {
            args.push("--slot");
            args.push(slot);
        }
        args.extend(base.iter().map(String::as_str));
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("az", &args, CommandOpts::default())
            .with_context(|| format!("failed to run az webapp config appsettings set for {key}"))?;

        output.check("az webapp config appsettings set", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let azure_app = self.resolve_app(target)?;
        let rg = &self.target_config.resource_group;
        let base = self.base_args();

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "webapp",
            "config",
            "appsettings",
            "delete",
            "--name",
            azure_app,
            "--resource-group",
            rg,
            "--setting-names",
            key,
        ];
        if let Some(slot) = self.resolve_slot(&target.environment) {
            args.push("--slot");
            args.push(slot);
        }
        args.extend(base.iter().map(String::as_str));
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("az", &args, CommandOpts::default())
            .with_context(|| {
                format!("failed to run az webapp config appsettings delete for {key}")
            })?;

        output.check("az webapp config appsettings delete", key)
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
targets:
  azure_app_service:
    resource_group: my-resource-group
    app_names:
      web: my-azure-webapp
    slot:
      staging: staging
    subscription: my-sub-id
    env_flags:
      prod: "--debug"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "azure_app_service".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.50.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"{}".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(
            calls[1].args,
            vec!["account", "show", "--subscription", "my-sub-id"]
        );
    }

    #[test]
    fn preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.50.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not logged in".to_vec(),
            },
        ]);
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("az is not authenticated"));
    }

    #[test]
    fn preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("az is not installed"));
    }

    #[test]
    fn deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "az");
        assert_eq!(
            calls[0].args,
            vec![
                "webapp",
                "config",
                "appsettings",
                "set",
                "--name",
                "my-azure-webapp",
                "--resource-group",
                "my-resource-group",
                "--settings",
                "MY_KEY=secret_val",
                "--subscription",
                "my-sub-id",
            ]
        );
    }

    #[test]
    fn deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--debug".to_string()));
    }

    #[test]
    fn deploy_with_slot() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "staging"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "webapp",
                "config",
                "appsettings",
                "set",
                "--name",
                "my-azure-webapp",
                "--resource-group",
                "my-resource-group",
                "--settings",
                "KEY=val",
                "--slot",
                "staging",
                "--subscription",
                "my-sub-id",
            ]
        );
    }

    #[test]
    fn delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AzureAppServiceTarget {
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
                "webapp",
                "config",
                "appsettings",
                "delete",
                "--name",
                "my-azure-webapp",
                "--resource-group",
                "my-resource-group",
                "--setting-names",
                "MY_KEY",
                "--subscription",
                "my-sub-id",
            ]
        );
    }

    #[test]
    fn requires_app() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = AzureAppServiceTarget {
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
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("api"), "dev"))
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("no azure_app_service app_names mapping"));
    }

    #[test]
    fn nonzero_exit() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.azure_app_service.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = AzureAppServiceTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
