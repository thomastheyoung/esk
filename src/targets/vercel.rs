//! Vercel target — deploys environment variables via the `vercel` CLI.
//!
//! Vercel is a cloud platform for frontend frameworks and serverless functions.
//! Environment variables are scoped to environments (production, preview,
//! development) and injected at build and runtime.
//!
//! CLI: `vercel` (Vercel's official CLI).
//! Commands: `vercel env add <key> <env> --force` / `vercel env rm <key> <env> --yes`.
//!
//! Secrets are sent via **stdin** to avoid process argument exposure. Vercel
//! uses its own environment names (production/preview/development), so esk
//! environment names are mapped via the `env_names` config field.

use anyhow::{Context, Result};

use crate::targets::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployTarget,
    DeployMode,
};
use crate::config::{Config, ResolvedTarget, VercelTargetConfig};

pub struct VercelTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a VercelTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> VercelTarget<'a> {
    fn resolve_env_name(&self, env: &str) -> Result<&str> {
        self.target_config
            .env_names
            .get(env)
            .map(|s| s.as_str())
            .with_context(|| format!("no vercel env_names mapping for '{env}'"))
    }
}

impl<'a> DeployTarget for VercelTarget<'a> {
    fn name(&self) -> &str {
        "vercel"
    }

    fn sync_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "vercel").map_err(|_| {
            anyhow::anyhow!(
                "vercel is not installed or not in PATH. Install it with: npm install -g vercel"
            )
        })?;
        let output = self
            .runner
            .run("vercel", &["whoami"], CommandOpts::default())
            .context("failed to run vercel whoami")?;
        if !output.success {
            anyhow::bail!("vercel is not authenticated. Run: vercel login");
        }
        Ok(())
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let vercel_env = self.resolve_env_name(&target.environment)?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env", "add", key, vercel_env, "--force"];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "vercel",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run vercel for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("vercel env add failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let vercel_env = self.resolve_env_name(&target.environment)?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env", "rm", key, vercel_env, "--yes"];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("vercel", &args, CommandOpts::default())
            .with_context(|| format!("failed to run vercel delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("vercel env rm failed for {key}: {stderr}");
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
  vercel:
    env_names:
      dev: development
      prod: production
    env_flags:
      prod: "--scope my-team"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "vercel".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn vercel_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"user".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
    }

    #[test]
    fn vercel_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
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
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("vercel is not authenticated"));
    }

    #[test]
    fn vercel_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("vercel is not installed"));
    }

    #[test]
    fn vercel_sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "vercel");
        assert_eq!(
            calls[0].1,
            vec!["env", "add", "MY_KEY", "development", "--force"]
        );
    }

    #[test]
    fn vercel_passes_value_via_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("KEY", "my_secret", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].2.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn vercel_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = VercelTarget {
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
            vec![
                "env",
                "add",
                "KEY",
                "production",
                "--force",
                "--scope",
                "my-team"
            ]
        );
    }

    #[test]
    fn vercel_missing_env_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .sync_secret("KEY", "val", &make_target("staging"))
            .unwrap_err();
        assert!(err.to_string().contains("no vercel env_names mapping"));
    }

    #[test]
    fn vercel_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "env",
                "rm",
                "MY_KEY",
                "production",
                "--yes",
                "--scope",
                "my-team"
            ]
        );
    }

    #[test]
    fn vercel_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = VercelTarget {
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
    fn vercel_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.vercel.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = VercelTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
