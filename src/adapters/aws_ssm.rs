use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SyncAdapter,
    SyncMode,
};
use crate::config::{AwsSsmAdapterConfig, Config, ResolvedTarget};

pub struct AwsSsmAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a AwsSsmAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> AwsSsmAdapter<'a> {
    fn resolve_path(&self, key: &str, target: &ResolvedTarget) -> String {
        let prefix = self
            .adapter_config
            .path_prefix
            .replace("{project}", &self.config.project)
            .replace("{environment}", &target.environment);
        format!("{prefix}{key}")
    }

    fn base_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(region) = &self.adapter_config.region {
            args.push("--region".to_string());
            args.push(region.clone());
        }
        if let Some(profile) = &self.adapter_config.profile {
            args.push("--profile".to_string());
            args.push(profile.clone());
        }
        args
    }
}

impl<'a> SyncAdapter for AwsSsmAdapter<'a> {
    fn name(&self) -> &str {
        "aws_ssm"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
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
        let param_type = &self.adapter_config.parameter_type;
        let base = self.base_args();

        // Use --cli-input-json via stdin to avoid exposing value in ps output
        let input_json = serde_json::json!({
            "Name": param_path,
            "Value": value,
            "Type": param_type,
            "Overwrite": true,
        });

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
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

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
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
    use crate::adapters::{CommandOpts, CommandOutput, CommandRunner};
    use std::sync::Mutex;

    type RunnerCall = (String, Vec<String>, Option<Vec<u8>>);

    struct MockRunner {
        calls: Mutex<Vec<RunnerCall>>,
        responses: Mutex<Vec<CommandOutput>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }
        fn take_calls(&self) -> Vec<RunnerCall> {
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
            adapter: "aws_ssm".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
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
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not authenticated"));
    }

    #[test]
    fn preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &FailRunner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not installed"));
    }

    #[test]
    fn sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
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
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].1.contains(&"--no-paginate".to_string()));
    }

    #[test]
    fn delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
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
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn sync_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"access denied".to_vec(),
        }]);
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn path_interpolation() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.aws_ssm.as_ref().unwrap();
        let adapter = AwsSsmAdapter {
            config: &config,
            adapter_config,
            runner: &MockRunner::new(vec![]),
        };
        let path = adapter.resolve_path("DB_PASSWORD", &make_target("prod"));
        assert_eq!(path, "/myapp/prod/DB_PASSWORD");
    }
}
