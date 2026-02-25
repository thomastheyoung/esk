use anyhow::{Context, Result};

use crate::targets::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployTarget,
    DeployMode,
};
use crate::config::{AwsSsmTargetConfig, Config, ResolvedTarget};

pub struct AwsSsmTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a AwsSsmTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> AwsSsmTarget<'a> {
    fn resolve_path(&self, key: &str, target: &ResolvedTarget) -> String {
        let prefix = self
            .target_config
            .path_prefix
            .replace("{project}", &self.config.project)
            .replace("{environment}", &target.environment);
        format!("{prefix}{key}")
    }

    fn base_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(region) = &self.target_config.region {
            args.push("--region".to_string());
            args.push(region.clone());
        }
        if let Some(profile) = &self.target_config.profile {
            args.push("--profile".to_string());
            args.push(profile.clone());
        }
        args
    }
}

impl<'a> DeployTarget for AwsSsmTarget<'a> {
    fn name(&self) -> &str {
        "aws_ssm"
    }

    fn sync_mode(&self) -> DeployMode {
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
        for a in &base {
            args.push(a);
        }
        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
            .context("failed to run aws sts get-caller-identity")?;
        if !output.success {
            anyhow::bail!("aws is not authenticated. Run: aws configure");
        }
        Ok(())
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
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
        for a in &base {
            args.push(a);
        }
        append_env_flags(&mut args, &flag_parts);

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

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("aws ssm put-parameter failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let param_path = self.resolve_path(key, target);
        let base = self.base_args();

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["ssm", "delete-parameter", "--name", &param_path];
        for a in &base {
            args.push(a);
        }
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
            .with_context(|| format!("failed to run aws ssm delete-parameter for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("aws ssm delete-parameter failed for {key}: {stderr}");
        }

        Ok(())
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
  aws_ssm:
    path_prefix: "/{project}/{environment}/"
    region: us-east-1
    env_flags:
      prod: "--no-paginate"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
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
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
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
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(
            calls[1].1,
            vec!["sts", "get-caller-identity", "--region", "us-east-1"]
        );
    }

    #[test]
    fn preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
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
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not authenticated"));
    }

    #[test]
    fn preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = AwsSsmTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not installed"));
    }

    #[test]
    fn sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "aws");
        assert_eq!(
            calls[0].1,
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
        let stdin = calls[0].2.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        assert_eq!(json["Name"], "/myapp/dev/MY_KEY");
        assert_eq!(json["Value"], "secret_val");
        assert_eq!(json["Type"], "SecureString");
        assert_eq!(json["Overwrite"], true);
    }

    #[test]
    fn sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert!(calls[0].1.contains(&"--no-paginate".to_string()));
    }

    #[test]
    fn delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
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
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = AwsSsmTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn sync_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"access denied".to_vec(),
        }]);
        let target = AwsSsmTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn path_interpolation() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let target = AwsSsmTarget {
            config: &config,
            target_config,
            runner: &MockCommandRunner::from_outputs(vec![]),
        };
        let path = target.resolve_path("DB_PASSWORD", &make_target("prod"));
        assert_eq!(path, "/myapp/prod/DB_PASSWORD");
    }
}
