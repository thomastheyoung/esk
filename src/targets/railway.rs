//! Railway target — deploys variables via the `railway` CLI.
//!
//! Railway is a cloud platform for deploying applications with managed
//! infrastructure. Variables are scoped to a service/environment and injected
//! at runtime.
//!
//! CLI: `railway` (Railway's official CLI).
//! Commands: `railway variables --set KEY=value` / `railway variables delete KEY`.
//!
//! The Railway CLI does **not** support stdin for secret values, so they are
//! passed as command-line arguments (visible in `ps` output). The CLI
//! determines the target project/service from the linked context (no explicit
//! app flag needed).

use anyhow::{Context, Result};

use crate::config::{Config, RailwayTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct RailwayTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a RailwayTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> DeployTarget for RailwayTarget<'a> {
    fn name(&self) -> &str {
        "railway"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "railway").map_err(|_| {
            anyhow::anyhow!(
                "railway is not installed or not in PATH. Install it from: https://docs.railway.app/guides/cli"
            )
        })?;
        let output = self
            .runner
            .run("railway", &["whoami"], CommandOpts::default())
            .context("failed to run railway whoami")?;
        if !output.success {
            anyhow::bail!("railway is not authenticated. Run: railway login");
        }
        Ok(())
    }

    // SECURITY: railway CLI has no stdin/file support for `variables --set`. Secret values are
    // exposed in process arguments (visible via `ps aux`). No workaround available.
    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let kv = format!("{key}={value}");

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variables", "--set", &kv];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("railway", &args, CommandOpts::default())
            .with_context(|| format!("failed to run railway for {key}"))?;

        output.check("railway variables --set", key)?;

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variables", "delete", key];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("railway", &args, CommandOpts::default())
            .with_context(|| format!("failed to run railway delete for {key}"))?;

        output.check("railway variables delete", key)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (String, Vec<String>);

    fn take_calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .take_calls()
            .into_iter()
            .map(|call| (call.program, call.args))
            .collect()
    }

    fn make_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: x
environments: [dev, prod]
targets:
  railway:
    env_flags:
      prod: "--environment production"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "railway".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn railway_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"3.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"user@test".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = RailwayTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls[1].1, vec!["whoami"]);
    }

    #[test]
    fn railway_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"3.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not logged in".to_vec(),
            },
        ]);
        let target = RailwayTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("railway is not authenticated"));
    }

    #[test]
    fn railway_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = RailwayTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("railway is not installed"));
    }

    #[test]
    fn railway_deploy_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "railway");
        assert_eq!(calls[0].1, vec!["variables", "--set", "MY_KEY=secret_val"]);
    }

    #[test]
    fn railway_deploy_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config: &config,
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
                "variables",
                "--set",
                "KEY=val",
                "--environment",
                "production"
            ]
        );
    }

    #[test]
    fn railway_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].1, vec!["variables", "delete", "MY_KEY"]);
    }

    #[test]
    fn railway_delete_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("KEY", &make_target("prod")).unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec!["variables", "delete", "KEY", "--environment", "production"]
        );
    }

    #[test]
    fn railway_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = RailwayTarget {
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
    fn railway_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"api error".to_vec(),
        }]);
        let target = RailwayTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("api error"));
    }
}
