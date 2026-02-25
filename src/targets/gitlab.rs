//! GitLab CI/CD target — deploys project variables via the `glab` CLI.
//!
//! GitLab CI/CD variables are injected into pipeline jobs. They can be scoped
//! to specific environments and optionally masked in job logs or protected
//! (only available on protected branches/tags).
//!
//! CLI: `glab` (GitLab's official CLI).
//! Commands: `glab variable set` / `glab variable delete`.
//!
//! Secrets are sent via **stdin** to avoid process argument exposure. Each
//! variable is scoped to an environment with `--scope <environment>`.

use anyhow::{Context, Result};

use crate::config::{Config, GitlabTargetConfig, ResolvedTarget};
use crate::targets::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode,
    DeployTarget,
};

pub struct GitlabTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a GitlabTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> DeployTarget for GitlabTarget<'a> {
    fn name(&self) -> &str {
        "gitlab"
    }

    fn sync_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "glab").map_err(|_| {
            anyhow::anyhow!(
                "glab is not installed or not in PATH. Install it from: https://gitlab.com/gitlab-org/cli"
            )
        })?;
        let output = self
            .runner
            .run("glab", &["auth", "status"], CommandOpts::default())
            .context("failed to run glab auth status")?;
        if !output.success {
            anyhow::bail!("glab is not authenticated. Run: glab auth login");
        }
        Ok(())
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variable", "set", key, "--scope", &target.environment];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "glab",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run glab for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("glab variable set failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variable", "delete", key, "--scope", &target.environment];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("glab", &args, CommandOpts::default())
            .with_context(|| format!("failed to run glab delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("glab variable delete failed for {key}: {stderr}");
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
project: x
environments: [dev, prod]
targets:
  gitlab:
    env_flags:
      prod: "--masked"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "gitlab".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn gitlab_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"Logged in".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls[1].1, vec!["auth", "status"]);
    }

    #[test]
    fn gitlab_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not logged in".to_vec(),
            },
        ]);
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("glab is not authenticated"));
    }

    #[test]
    fn gitlab_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("glab is not installed"));
    }

    #[test]
    fn gitlab_sync_uses_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "glab");
        assert_eq!(
            calls[0].1,
            vec!["variable", "set", "MY_KEY", "--scope", "dev"]
        );
        // Value is passed via stdin, not in args
        assert_eq!(calls[0].2.as_deref(), Some(b"secret_val".as_slice()));
        assert!(!calls[0].1.iter().any(|a| a.contains("secret_val")));
    }

    #[test]
    fn gitlab_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec!["variable", "set", "KEY", "--scope", "prod", "--masked"]
        );
        assert_eq!(calls[0].2.as_deref(), Some(b"val".as_slice()));
    }

    #[test]
    fn gitlab_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec!["variable", "delete", "MY_KEY", "--scope", "dev"]
        );
    }

    #[test]
    fn gitlab_delete_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("KEY", &make_target("prod")).unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec!["variable", "delete", "KEY", "--scope", "prod", "--masked"]
        );
    }

    #[test]
    fn gitlab_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = GitlabTarget {
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
    fn gitlab_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"api error".to_vec(),
        }]);
        let target = GitlabTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("api error"));
    }
}
