//! CircleCI target — deploys context secrets via the `circleci` CLI.
//!
//! CircleCI contexts are the primary mechanism for sharing secrets across
//! projects and pipelines. Secrets stored in a context are injected as
//! environment variables into jobs that reference that context.
//!
//! CLI: `circleci` (CircleCI's official CLI).
//! Commands: `circleci context store-secret` / `circleci context remove-secret`.
//!
//! Secrets are sent via **stdin** to avoid process argument exposure. Requires
//! `--org-id` and a context name to identify the target context.

use anyhow::{Context, Result};

use crate::config::{CircleciTargetConfig, Config, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct CircleciTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a CircleciTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for CircleciTarget<'_> {
    fn name(&self) -> &'static str {
        "circleci"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "circleci").map_err(|_| {
            anyhow::anyhow!(
                "circleci is not installed or not in PATH. Install it from: https://circleci.com/docs/local-cli/"
            )
        })?;
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let org_id = &self.target_config.org_id;
        let context = &self.target_config.context_name;
        let mut args: Vec<&str> = vec!["context", "store-secret", "--org-id", org_id, context, key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "circleci",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run circleci context store-secret for {key}"))?
            .check("circleci context store-secret", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let org_id = &self.target_config.org_id;
        let context = &self.target_config.context_name;
        let mut args: Vec<&str> =
            vec!["context", "remove-secret", "--org-id", org_id, context, key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("circleci", &args, CommandOpts::default())
            .with_context(|| format!("failed to run circleci context remove-secret for {key}"))?
            .check("circleci context remove-secret", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (String, Vec<String>, Option<Vec<u8>>);

    fn take_calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .take_calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.stdin))
            .collect()
    }

    fn make_config() -> ConfigFixture {
        let yaml = r#"
project: x
environments: [dev, prod]
targets:
  circleci:
    org_id: "00000000-0000-0000-0000-000000000000"
    context_name: my-context
    env_flags:
      prod: "--some-flag value"
"#;
        ConfigFixture::new(yaml).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "circleci".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn circleci_preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"0.1.0".to_vec(),
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
    }

    #[test]
    fn circleci_preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("circleci is not installed"));
    }

    #[test]
    fn circleci_deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "circleci");
        assert_eq!(
            calls[0].1,
            vec![
                "context",
                "store-secret",
                "--org-id",
                "00000000-0000-0000-0000-000000000000",
                "my-context",
                "MY_KEY"
            ]
        );
    }

    #[test]
    fn circleci_passes_value_via_stdin() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "my_secret", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].2.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn circleci_deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "context",
                "store-secret",
                "--org-id",
                "00000000-0000-0000-0000-000000000000",
                "my-context",
                "KEY",
                "--some-flag",
                "value"
            ]
        );
    }

    #[test]
    fn circleci_delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "context",
                "remove-secret",
                "--org-id",
                "00000000-0000-0000-0000-000000000000",
                "my-context",
                "MY_KEY"
            ]
        );
        // No stdin for delete
        assert!(calls[0].2.is_none());
    }

    #[test]
    fn circleci_deploy_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }

    #[test]
    fn circleci_delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
