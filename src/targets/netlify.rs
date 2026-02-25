use anyhow::{Context, Result};

use crate::targets::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployTarget,
    DeployMode,
};
use crate::config::{Config, NetlifyTargetConfig, ResolvedTarget};

pub struct NetlifyTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a NetlifyTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> DeployTarget for NetlifyTarget<'a> {
    fn name(&self) -> &str {
        "netlify"
    }

    fn sync_mode(&self) -> DeployMode {
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
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:set", key, value];
        if let Some(site) = &self.target_config.site {
            args.push("--site");
            args.push(site);
        }
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("netlify", &args, CommandOpts::default())
            .with_context(|| format!("failed to run netlify for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("netlify env:set failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:unset", key];
        if let Some(site) = &self.target_config.site {
            args.push("--site");
            args.push(site);
        }
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("netlify", &args, CommandOpts::default())
            .with_context(|| format!("failed to run netlify delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("netlify env:unset failed for {key}: {stderr}");
        }

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
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls[1].1, vec!["status"]);
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
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("netlify is not linked"));
    }

    #[test]
    fn netlify_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("netlify is not installed"));
    }

    #[test]
    fn netlify_sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "netlify");
        assert_eq!(calls[0].1, vec!["env:set", "MY_KEY", "secret_val"]);
    }

    #[test]
    fn netlify_sync_with_site() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec!["env:set", "KEY", "val", "--site", "my-site-id"]
        );
    }

    #[test]
    fn netlify_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
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
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
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
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = adapter
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
        let adapter = NetlifyTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
