//! AWS Systems Manager Parameter Store target — deploys secrets via the `aws` CLI.
//!
//! SSM Parameter Store is a key-value store within AWS Systems Manager for
//! configuration data and secrets. Parameters are organized in a hierarchy
//! (e.g. `/{project}/{env}/KEY`) and can be encrypted with KMS (`SecureString`
//! type).
//!
//! CLI: `aws` (AWS CLI v2).
//! Commands: `aws ssm put-parameter` / `aws ssm delete-parameter`.
//!
//! Parameters are created via `--cli-input-json` with the JSON payload on
//! **stdin** to avoid exposing secret values in process arguments. Supports
//! `--region` and `--profile` flags for multi-account setups. The
//! `parameter_type` config field controls the SSM type (default: `SecureString`).

use anyhow::{Context, Result};

use crate::config::{AwsSsmTargetConfig, Config, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct AwsSsmTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a AwsSsmTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl AwsSsmTarget<'_> {
    fn resolve_path(&self, key: &str, target: &ResolvedTarget) -> String {
        let prefix = self
            .target_config
            .path_prefix
            .replace("{project}", &self.config.project)
            .replace("{environment}", &target.environment);
        format!("{prefix}{key}")
    }

    fn base_args(&self) -> Vec<String> {
        crate::targets::aws_base_args(
            self.target_config.region.as_deref(),
            self.target_config.profile.as_deref(),
        )
    }
}

impl DeployTarget for AwsSsmTarget<'_> {
    fn name(&self) -> &'static str {
        "aws_ssm"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "aws").map_err(|_| {
            anyhow::anyhow!(
                "aws is not installed or not in PATH. Install it from: https://aws.amazon.com/cli/"
            )
        })?;
        let base = self.base_args();
        let mut args: Vec<&str> = vec!["sts", "get-caller-identity"];
        args.extend(base.iter().map(String::as_str));
        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
            .context("failed to run aws sts get-caller-identity")?;
        if !output.success {
            anyhow::bail!("aws is not authenticated. Run: aws configure");
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let param_path = self.resolve_path(key, target);
        let param_type = &self.target_config.parameter_type;
        let base = self.base_args();

        // Use --cli-input-json via stdin to avoid exposing value in ps output
        let input_json = serde_json::json!({
            "Name": param_path,
            "Value": value,
            "Type": param_type,
            "Overwrite": true,
        });

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "ssm",
            "put-parameter",
            "--cli-input-json",
            "file:///dev/stdin",
        ];
        args.extend(base.iter().map(String::as_str));
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run(
                "aws",
                &args,
                CommandOpts {
                    stdin: Some(input_json.to_string().into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run aws ssm put-parameter for {key}"))?;

        output.check("aws ssm put-parameter", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let param_path = self.resolve_path(key, target);
        let base = self.base_args();

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["ssm", "delete-parameter", "--name", &param_path];
        args.extend(base.iter().map(String::as_str));
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
            .with_context(|| format!("failed to run aws ssm delete-parameter for {key}"))?;

        output.check("aws ssm delete-parameter", key)
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
  aws_ssm:
    path_prefix: "/{project}/{environment}/"
    region: us-east-1
    env_flags:
      prod: "--no-paginate"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "aws_ssm".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"{}".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(
            calls[1].args,
            vec!["sts", "get-caller-identity", "--region", "us-east-1"]
        );
    }

    #[test]
    fn preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not configured".to_vec(),
            },
        ]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not authenticated"));
    }

    #[test]
    fn preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not installed"));
    }

    #[test]
    fn deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "aws");
        assert_eq!(
            calls[0].args,
            vec![
                "ssm",
                "put-parameter",
                "--cli-input-json",
                "file:///dev/stdin",
                "--region",
                "us-east-1"
            ]
        );
        // Verify stdin contains the JSON payload
        let stdin = calls[0].stdin.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        assert_eq!(json["Name"], "/myapp/dev/MY_KEY");
        assert_eq!(json["Value"], "secret_val");
        assert_eq!(json["Type"], "SecureString");
        assert_eq!(json["Overwrite"], true);
    }

    #[test]
    fn deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--no-paginate".to_string()));
    }

    #[test]
    fn delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "ssm",
                "delete-parameter",
                "--name",
                "/myapp/dev/MY_KEY",
                "--region",
                "us-east-1"
            ]
        );
    }

    #[test]
    fn delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn deploy_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"access denied".to_vec(),
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn path_interpolation() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &MockCommandRunner::from_outputs(vec![]),
        };
        let path = target.resolve_path("DB_PASSWORD", &make_target("prod"));
        assert_eq!(path, "/myapp/prod/DB_PASSWORD");
    }
}
