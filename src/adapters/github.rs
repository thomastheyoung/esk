use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SyncAdapter,
    SyncMode,
};
use crate::config::{Config, GithubAdapterConfig, ResolvedTarget};

pub struct GithubAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a GithubAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> SyncAdapter for GithubAdapter<'a> {
    fn name(&self) -> &str {
        "github"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "gh").map_err(|_| {
            anyhow::anyhow!(
                "gh is not installed or not in PATH. Install it from: https://cli.github.com/"
            )
        })?;
        let output = self
            .runner
            .run("gh", &["auth", "status"], CommandOpts::default())
            .context("failed to run gh auth status")?;
        if !output.success {
            anyhow::bail!("gh is not authenticated. Run: gh auth login");
        }
        Ok(())
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "set", key];
        if let Some(repo) = &self.adapter_config.repo {
            args.push("-R");
            args.push(repo);
        }
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "gh",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run gh for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh secret set failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "delete", key];
        if let Some(repo) = &self.adapter_config.repo {
            args.push("-R");
            args.push(repo);
        }
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("gh", &args, CommandOpts::default())
            .with_context(|| format!("failed to run gh delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh secret delete failed for {key}: {stderr}");
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
        calls: Mutex<Vec<(String, Vec<String>, Option<Vec<u8>>)>>,
        responses: Mutex<Vec<CommandOutput>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }
        fn take_calls(&self) -> Vec<(String, Vec<String>, Option<Vec<u8>>)> {
            std::mem::take(&mut *self.calls.lock().unwrap())
        }
    }

    impl CommandRunner for MockRunner {
        fn run(
            &self,
            program: &str,
            args: &[&str],
            opts: CommandOpts,
        ) -> anyhow::Result<CommandOutput> {
            self.calls.lock().unwrap().push((
                program.to_string(),
                args.iter().map(|s| s.to_string()).collect(),
                opts.stdin,
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

    fn make_config(dir: &std::path::Path, with_repo: bool) -> Config {
        let yaml = if with_repo {
            r#"
project: x
environments: [dev, prod]
adapters:
  github:
    repo: owner/repo
    env_flags:
      prod: "--env production"
"#
        } else {
            r#"
project: x
environments: [dev, prod]
adapters:
  github:
    env_flags:
      prod: "--env production"
"#
        };
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            adapter: "github".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn github_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![
            CommandOutput { success: true, stdout: b"2.0.0".to_vec(), stderr: vec![] },
            CommandOutput { success: true, stdout: b"Logged in".to_vec(), stderr: vec![] },
        ]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].1, vec!["auth", "status"]);
    }

    #[test]
    fn github_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![
            CommandOutput { success: true, stdout: b"2.0.0".to_vec(), stderr: vec![] },
            CommandOutput { success: false, stdout: vec![], stderr: b"not logged in".to_vec() },
        ]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("gh is not authenticated"));
    }

    #[test]
    fn github_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &FailRunner };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("gh is not installed"));
    }

    #[test]
    fn github_sync_correct_args_with_repo() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        adapter.sync_secret("MY_KEY", "secret_val", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].0, "gh");
        assert_eq!(calls[0].1, vec!["secret", "set", "MY_KEY", "-R", "owner/repo"]);
    }

    #[test]
    fn github_passes_value_via_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        adapter.sync_secret("KEY", "my_secret", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].2.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn github_sync_without_repo() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        adapter.sync_secret("KEY", "val", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].1, vec!["secret", "set", "KEY"]);
    }

    #[test]
    fn github_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        adapter.sync_secret("KEY", "val", &make_target("prod")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].1, vec!["secret", "set", "KEY", "-R", "owner/repo", "--env", "production"]);
    }

    #[test]
    fn github_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: true, stdout: vec![], stderr: vec![] }]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        adapter.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].1, vec!["secret", "delete", "MY_KEY", "-R", "owner/repo"]);
    }

    #[test]
    fn github_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: false, stdout: vec![], stderr: b"not found".to_vec() }]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.delete_secret("KEY", &make_target("dev")).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn github_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput { success: false, stdout: vec![], stderr: b"auth error".to_vec() }]);
        let adapter = GithubAdapter { config: &config, adapter_config, runner: &runner };
        let err = adapter.sync_secret("KEY", "val", &make_target("dev")).unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
