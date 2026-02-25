//! Heroku target — deploys config vars via the `heroku` CLI.
//!
//! Heroku is a cloud PaaS that runs applications in managed containers (dynos).
//! Config vars are exposed as environment variables to the running application
//! and persist across deploys.
//!
//! CLI: `heroku` (Heroku's official CLI).
//! Commands: `heroku config:set KEY=value -a <app>` / `heroku config:unset KEY -a <app>`.
//!
//! The Heroku CLI does **not** support stdin for secret values, so they are
//! passed as command-line arguments (visible in `ps` output). Requires an app
//! name (mapped from esk's app config).

use anyhow::{Context, Result};

use crate::config::{Config, HerokuTargetConfig, ResolvedTarget};
use crate::targets::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode,
    DeployTarget,
};

pub struct HerokuTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a HerokuTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> HerokuTarget<'a> {
    fn resolve_app(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("heroku target requires an app")?;
        self.target_config
            .app_names
            .get(app)
            .map(|s| s.as_str())
            .with_context(|| format!("no heroku app_names mapping for '{app}'"))
    }
}

impl<'a> DeployTarget for HerokuTarget<'a> {
    fn name(&self) -> &str {
        "heroku"
    }

    fn sync_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "heroku").map_err(|_| {
            anyhow::anyhow!(
                "heroku is not installed or not in PATH. Install it from: https://devcenter.heroku.com/articles/heroku-cli"
            )
        })?;
        let output = self
            .runner
            .run("heroku", &["auth:whoami"], CommandOpts::default())
            .context("failed to run heroku auth:whoami")?;
        if !output.success {
            anyhow::bail!("heroku is not authenticated. Run: heroku login");
        }
        Ok(())
    }

    // SECURITY: heroku CLI has no stdin/file support for config:set. Secret values are exposed
    // in process arguments (visible via `ps aux`). Feature requested upstream since 2016, never
    // implemented. No workaround available.
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let heroku_app = self.resolve_app(target)?;
        let kv = format!("{key}={value}");

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["config:set", &kv, "-a", heroku_app];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("heroku", &args, CommandOpts::default())
            .with_context(|| format!("failed to run heroku for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("heroku config:set failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let heroku_app = self.resolve_app(target)?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["config:unset", key, "-a", heroku_app];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("heroku", &args, CommandOpts::default())
            .with_context(|| format!("failed to run heroku delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("heroku config:unset failed for {key}: {stderr}");
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

    fn make_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
targets:
  heroku:
    app_names:
      web: my-heroku-app
    env_flags:
      prod: "--remote staging"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "heroku".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn heroku_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"user@test".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls[1].1, vec!["auth:whoami"]);
    }

    #[test]
    fn heroku_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
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
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("heroku is not authenticated"));
    }

    #[test]
    fn heroku_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("heroku is not installed"));
    }

    #[test]
    fn heroku_sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "heroku");
        assert_eq!(
            calls[0].1,
            vec!["config:set", "MY_KEY=secret_val", "-a", "my-heroku-app"]
        );
    }

    #[test]
    fn heroku_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .sync_secret("KEY", "val", &make_target(Some("web"), "prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "config:set",
                "KEY=val",
                "-a",
                "my-heroku-app",
                "--remote",
                "staging"
            ]
        );
    }

    #[test]
    fn heroku_requires_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .sync_secret("KEY", "val", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("requires an app"));
    }

    #[test]
    fn heroku_unknown_app_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .sync_secret("KEY", "val", &make_target(Some("api"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("no heroku app_names mapping"));
    }

    #[test]
    fn heroku_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec!["config:unset", "MY_KEY", "-a", "my-heroku-app"]
        );
    }

    #[test]
    fn heroku_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn heroku_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = HerokuTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .sync_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
