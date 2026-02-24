use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, validate_stdin_kv_value, CommandOpts,
    CommandRunner, SyncAdapter, SyncMode,
};
use crate::config::{Config, FlyAdapterConfig, ResolvedTarget};

pub struct FlyAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a FlyAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> FlyAdapter<'a> {
    fn resolve_app(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("fly adapter requires an app")?;
        self.adapter_config
            .app_names
            .get(app)
            .map(|s| s.as_str())
            .with_context(|| format!("no fly app_names mapping for '{app}'"))
    }
}

impl<'a> SyncAdapter for FlyAdapter<'a> {
    fn name(&self) -> &str {
        "fly"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "fly").map_err(|_| {
            anyhow::anyhow!(
                "fly is not installed or not in PATH. Install it from: https://fly.io/docs/hands-on/install-flyctl/"
            )
        })?;
        let output = self
            .runner
            .run("fly", &["auth", "whoami"], CommandOpts::default())
            .context("failed to run fly auth whoami")?;
        if !output.success {
            anyhow::bail!("fly is not authenticated. Run: fly auth login");
        }
        Ok(())
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        validate_stdin_kv_value(key, value, "fly")?;
        let fly_app = self.resolve_app(target)?;
        let stdin_data = format!("{key}={value}\n");

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "import", "-a", fly_app];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "fly",
                &args,
                CommandOpts {
                    stdin: Some(stdin_data.into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run fly for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("fly secrets import failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let fly_app = self.resolve_app(target)?;

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "unset", key, "-a", fly_app];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("fly", &args, CommandOpts::default())
            .with_context(|| format!("failed to run fly delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("fly secrets unset failed for {key}: {stderr}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

    fn make_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
adapters:
  fly:
    app_names:
      web: my-fly-app
    env_flags:
      prod: "--stage"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            adapter: "fly".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn fly_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
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
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["auth", "whoami"]);
    }

    #[test]
    fn fly_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
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
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("fly is not authenticated"));
    }

    #[test]
    fn fly_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("fly is not installed"));
    }

    #[test]
    fn fly_sync_uses_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "fly");
        assert_eq!(calls[0].args, vec!["secrets", "import", "-a", "my-fly-app"]);
        // Value is passed via stdin, not in args
        assert_eq!(
            calls[0].stdin.as_deref(),
            Some(b"MY_KEY=secret_val\n".as_slice())
        );
        assert!(!calls[0].args.iter().any(|a| a.contains("secret_val")));
    }

    #[test]
    fn fly_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target(Some("web"), "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["secrets", "import", "-a", "my-fly-app", "--stage"]
        );
        assert_eq!(calls[0].stdin.as_deref(), Some(b"KEY=val\n".as_slice()));
    }

    #[test]
    fn fly_requires_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("requires an app"));
    }

    #[test]
    fn fly_unknown_app_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target(Some("api"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("no fly app_names mapping"));
    }

    #[test]
    fn fly_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["secrets", "unset", "MY_KEY", "-a", "my-fly-app"]
        );
    }

    #[test]
    fn fly_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .delete_secret("KEY", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn fly_rejects_newline_in_value() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "line1\nline2", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn fly_rejects_cr_in_value() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "line1\r\nline2", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn fly_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"deploy error".to_vec(),
        }]);
        let adapter = FlyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("deploy error"));
    }
}
