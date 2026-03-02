//! Netlify target — deploys environment variables via the `netlify` CLI.
//!
//! Netlify is a web hosting and automation platform for modern web projects.
//! Environment variables are available during builds and in Netlify Functions
//! (serverless).
//!
//! CLI: `netlify` (Netlify's official CLI).
//! Commands: `netlify env:set` / `netlify env:unset`.
//!
//! The Netlify CLI does **not** support stdin or file input for secret values,
//! so they are passed as command-line arguments (visible in `ps` output).
//! Supports an optional `--site` flag to target a specific site.

use anyhow::{Context, Result};

use crate::config::{Config, NetlifyTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct NetlifyTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a NetlifyTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for NetlifyTarget<'_> {
    fn name(&self) -> &'static str {
        "netlify"
    }

    fn passes_value_as_cli_arg(&self) -> bool {
        true
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "netlify").map_err(|_| {
            anyhow::anyhow!(
                "netlify is not installed or not in PATH. Install it with: npm install -g netlify-cli"
            )
        })?;
        let output = self
            .runner
            .run("netlify", &["status"], CommandOpts::default())
            .context("failed to run netlify status")?;
        if !output.success {
            anyhow::bail!("netlify is not linked. Run: netlify link");
        }
        Ok(())
    }

    // SECURITY: netlify CLI has no stdin/file support for `env:set`. It has `env:import` but with
    // different semantics (replaces all vars). Secret values are exposed in process arguments
    // (visible via `ps aux`). No workaround available.
    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:set", key, value];
        if let Some(site) = &self.target_config.site {
            args.push("--site");
            args.push(site);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("netlify", &args, CommandOpts::default())
            .with_context(|| format!("failed to run netlify env:set for {key}"))?
            .check("netlify env:set", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:unset", key];
        if let Some(site) = &self.target_config.site {
            args.push("--site");
            args.push(site);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("netlify", &args, CommandOpts::default())
            .with_context(|| format!("failed to run netlify env:unset for {key}"))?
            .check("netlify env:unset", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};



    fn make_config(dir: &std::path::Path, with_site: bool) -> Config {
        let yaml = if with_site {
            r#"
project: x
environments: [dev, prod]
targets:
  netlify:
    site: my-site-id
    env_flags:
      prod: "--context production"
"#
        } else {
            r#"
project: x
environments: [dev, prod]
targets:
  netlify:
    env_flags:
      prod: "--context production"
"#
        };
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "netlify".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn netlify_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"linked".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].args, vec!["status"]);
    }

    #[test]
    fn netlify_preflight_not_linked() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not linked".to_vec(),
            },
        ]);
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("netlify is not linked"));
    }

    #[test]
    fn netlify_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("netlify is not installed"));
    }

    #[test]
    fn netlify_deploy_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "netlify");
        assert_eq!(calls[0].args, vec!["env:set", "MY_KEY", "secret_val"]);
    }

    #[test]
    fn netlify_deploy_with_site() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["env:set", "KEY", "val", "--site", "my-site-id"]
        );
    }

    #[test]
    fn netlify_deploy_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["env:set", "KEY", "val", "--context", "production"]
        );
    }

    #[test]
    fn netlify_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["env:unset", "MY_KEY", "--site", "my-site-id"]
        );
    }

    #[test]
    fn netlify_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = NetlifyTarget {
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
    fn netlify_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
