use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SyncAdapter,
    SyncMode,
};
use crate::config::{Config, RailwayAdapterConfig, ResolvedTarget};

pub struct RailwayAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a RailwayAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> SyncAdapter for RailwayAdapter<'a> {
    fn name(&self) -> &str {
        "railway"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
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

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let kv = format!("{key}={value}");

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variables", "--set", &kv];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("railway", &args, CommandOpts::default())
            .with_context(|| format!("failed to run railway for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("railway variables --set failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variables", "delete", key];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("railway", &args, CommandOpts::default())
            .with_context(|| format!("failed to run railway delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("railway variables delete failed for {key}: {stderr}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{CommandOpts, CommandOutput, CommandRunner};
    use std::sync::Mutex;

    struct MockRunner {
        calls: Mutex<Vec<(String, Vec<String>)>>,
        responses: Mutex<Vec<CommandOutput>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }
        fn take_calls(&self) -> Vec<(String, Vec<String>)> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl CommandRunner for MockRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            _opts: CommandOpts,
        ) -> anyhow::Result<CommandOutput> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
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
project: x
environments: [dev, prod]
adapters:
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
            adapter: "railway".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn railway_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].1, vec!["whoami"]);
    }

    #[test]
    fn railway_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("railway is not authenticated"));
    }

    #[test]
    fn railway_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &FailRunner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("railway is not installed"));
    }

    #[test]
    fn railway_sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].0, "railway");
        assert_eq!(calls[0].1, vec!["variables", "--set", "MY_KEY=secret_val"]);
    }

    #[test]
    fn railway_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
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
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].1, vec!["variables", "delete", "MY_KEY"]);
    }

    #[test]
    fn railway_delete_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter.delete_secret("KEY", &make_target("prod")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].1,
            vec!["variables", "delete", "KEY", "--environment", "production"]
        );
    }

    #[test]
    fn railway_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = RailwayAdapter {
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
    fn railway_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.railway.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"api error".to_vec(),
        }]);
        let adapter = RailwayAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("api error"));
    }
}
