use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SyncAdapter,
    SyncMode,
};
use crate::config::{Config, HerokuAdapterConfig, ResolvedTarget};

pub struct HerokuAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a HerokuAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> HerokuAdapter<'a> {
    fn resolve_app(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("heroku adapter requires an app")?;
        self.adapter_config
            .app_names
            .get(app)
            .map(|s| s.as_str())
            .with_context(|| format!("no heroku app_names mapping for '{app}'"))
    }
}

impl<'a> SyncAdapter for HerokuAdapter<'a> {
    fn name(&self) -> &str {
        "heroku"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
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

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let heroku_app = self.resolve_app(target)?;
        let kv = format!("{key}={value}");

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
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

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
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
apps:
  web:
    path: apps/web
adapters:
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
            adapter: "heroku".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn heroku_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![
            CommandOutput { success: true, stdout: b"1.0.0".to_vec(), stderr: vec![] },
            CommandOutput { success: true, stdout: b"user@test".to_vec(), stderr: vec![] },
        ]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].1, vec!["auth:whoami"]);
    }

    #[test]
    fn heroku_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![
            CommandOutput { success: true, stdout: b"1.0.0".to_vec(), stderr: vec![] },
            CommandOutput { success: false, stdout: vec![], stderr: b"not logged in".to_vec() },
        ]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("heroku is not authenticated"));
    }

    #[test]
    fn heroku_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &FailRunner };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("heroku is not installed"));
    }

    #[test]
    fn heroku_sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        adapter.sync_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].0, "heroku");
        assert_eq!(calls[0].1, vec!["config:set", "MY_KEY=secret_val", "-a", "my-heroku-app"]);
    }

    #[test]
    fn heroku_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        adapter.sync_secret("KEY", "val", &make_target(Some("web"), "prod")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].1, vec!["config:set", "KEY=val", "-a", "my-heroku-app", "--remote", "staging"]);
    }

    #[test]
    fn heroku_requires_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.sync_secret("KEY", "val", &make_target(None, "dev")).unwrap_err();
        assert!(err.to_string().contains("requires an app"));
    }

    #[test]
    fn heroku_unknown_app_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.sync_secret("KEY", "val", &make_target(Some("api"), "dev")).unwrap_err();
        assert!(err.to_string().contains("no heroku app_names mapping"));
    }

    #[test]
    fn heroku_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        adapter.delete_secret("MY_KEY", &make_target(Some("web"), "dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].1, vec!["config:unset", "MY_KEY", "-a", "my-heroku-app"]);
    }

    #[test]
    fn heroku_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: false, stdout: vec![], stderr: b"not found".to_vec() }]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.delete_secret("KEY", &make_target(Some("web"), "dev")).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn heroku_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.heroku.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: false, stdout: vec![], stderr: b"auth error".to_vec() }]);
        let adapter = HerokuAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.sync_secret("KEY", "val", &make_target(Some("web"), "dev")).unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
