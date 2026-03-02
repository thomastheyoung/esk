//! Convex target — deploys environment variables via `npx convex`.
//!
//! Convex is a backend-as-a-service platform with a real-time database and
//! serverless functions. Environment variables are set per-deployment and
//! are available to Convex functions at runtime.
//!
//! CLI: `npx convex` (runs via npx, no global install needed).
//! Commands: `convex env set` / `convex env unset`.
//!
//! Secrets are set via **stdin** — when `convex env set NAME` receives piped
//! input (non-TTY stdin), it reads the value from stdin. This avoids exposing
//! secret values in process arguments. The `CONVEX_DEPLOYMENT` environment
//! variable is read from the project's Convex config file and injected into
//! the command environment.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::config::{Config, ConvexTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct ConvexTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a ConvexTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl ConvexTarget<'_> {
    /// Resolve the cwd and env vars needed for convex commands.
    fn resolve_deployment_context(&self) -> Result<(PathBuf, Vec<(String, String)>)> {
        let cwd = self.config.root.join(&self.target_config.path);
        let mut env_vars: Vec<(String, String)> = Vec::new();

        if let Some(source) = &self.target_config.deployment_source {
            let source_path = self.config.root.join(source);
            if source_path.is_file() {
                let contents = std::fs::read_to_string(&source_path)
                    .with_context(|| format!("failed to read {}", source_path.display()))?;
                for line in contents.lines() {
                    if let Some(deployment) = line.strip_prefix("CONVEX_DEPLOYMENT=") {
                        let deployment = deployment.trim().trim_matches('"').trim_matches('\'');
                        env_vars.push(("CONVEX_DEPLOYMENT".to_string(), deployment.to_string()));
                        break;
                    }
                }
            }
        }

        Ok((cwd, env_vars))
    }
}

impl DeployTarget for ConvexTarget<'_> {
    fn name(&self) -> &'static str {
        "convex"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "npx").map_err(|_| {
            anyhow::anyhow!("npx is not installed or not in PATH. Install Node.js to get npx.")
        })?;
        let (cwd, env_vars) = self.resolve_deployment_context()?;
        let output = self
            .runner
            .run(
                "npx",
                &["convex", "env", "list"],
                CommandOpts {
                    cwd: Some(cwd),
                    env: env_vars,
                    ..Default::default()
                },
            )
            .context("failed to run convex env list")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("convex deployment not accessible: {stderr}");
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let (cwd, env_vars) = self.resolve_deployment_context()?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["convex", "env", "set", key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "npx",
                &args,
                CommandOpts {
                    cwd: Some(cwd),
                    env: env_vars,
                    stdin: Some(value.as_bytes().to_vec()),
                },
            )
            .with_context(|| format!("failed to run convex env set for {key}"))?
            .check("convex env set", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let (cwd, env_vars) = self.resolve_deployment_context()?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["convex", "env", "unset", key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "npx",
                &args,
                CommandOpts {
                    cwd: Some(cwd),
                    env: env_vars,
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run convex env unset for {key}"))?
            .check("convex env unset", key)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};



    fn make_config(dir: &std::path::Path, deployment_source: Option<&str>) -> Config {
        let mut yaml = String::from(
            r"
project: x
environments: [dev, prod]
targets:
  convex:
    path: apps/api
",
        );
        if let Some(s) = deployment_source {
            let _ = writeln!(yaml, "    deployment_source: {s}");
        }
        yaml.push_str("    env_flags:\n      prod: \"--prod\"\n");
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "convex".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn convex_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"10.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"KEY=value".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["convex", "env", "list"]);
    }

    #[test]
    fn convex_preflight_deployment_inaccessible() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"10.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"deployment not found".to_vec(),
            },
        ]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("convex deployment not accessible"));
        assert!(err.to_string().contains("deployment not found"));
    }

    #[test]
    fn convex_preflight_missing_npx() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), None);
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();

        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("npx is not installed"));
        assert!(err.to_string().contains("Node.js"));
    }

    #[test]
    fn convex_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "my_value", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "npx");
        assert_eq!(calls[0].args, vec!["convex", "env", "set", "MY_KEY"]);
        assert_eq!(calls[0].cwd.as_ref().unwrap(), &dir.path().join("apps/api"));
        // Value is passed via stdin, not in args
        assert_eq!(calls[0].stdin.as_deref(), Some(b"my_value".as_slice()));
        assert!(!calls[0].args.iter().any(|a| a.contains("my_value")));
    }

    #[test]
    fn convex_reads_deployment_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let source = dir.path().join("apps/api/.env.local");
        std::fs::write(&source, "CONVEX_DEPLOYMENT=dev:my-deploy-123\n").unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].env.contains(&(
            "CONVEX_DEPLOYMENT".to_string(),
            "dev:my-deploy-123".to_string()
        )));
    }

    #[test]
    fn convex_deployment_source_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].env.is_empty()); // no env vars set
    }

    #[test]
    fn convex_deployment_source_no_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let source = dir.path().join("apps/api/.env.local");
        std::fs::write(&source, "OTHER_VAR=foo\nSOMETHING=bar\n").unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].env.is_empty());
    }

    #[test]
    fn convex_deployment_strips_quotes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let source = dir.path().join("apps/api/.env.local");
        std::fs::write(&source, "CONVEX_DEPLOYMENT=\"my-deploy\"\n").unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0]
            .env
            .contains(&("CONVEX_DEPLOYMENT".to_string(), "my-deploy".to_string())));
    }

    #[test]
    fn convex_delete_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target("prod"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "npx");
        assert_eq!(
            calls[0].args,
            vec!["convex", "env", "unset", "MY_KEY", "--prod"]
        );
    }

    #[test]
    fn convex_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = ConvexTarget {
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
    fn convex_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"deploy error".to_vec(),
        }]);
        let target = ConvexTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("deploy error"));
    }
}
