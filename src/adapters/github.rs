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
    use crate::adapters::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (String, Vec<String>, Option<Vec<u8>>);

    fn take_calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .take_calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.stdin))
            .collect()
    }

    fn make_config(with_repo: bool) -> ConfigFixture {
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
        ConfigFixture::new(yaml).unwrap()
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
        let fixture = make_config(true);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"Logged in".to_vec(),
                stderr: vec![],
            },
        ]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls[1].1, vec!["auth", "status"]);
    }

    #[test]
    fn github_preflight_auth_failure() {
        let fixture = make_config(true);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not logged in".to_vec(),
            },
        ]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("gh is not authenticated"));
    }

    #[test]
    fn github_preflight_missing_cli() {
        let fixture = make_config(false);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("gh is not installed"));
    }

    #[test]
    fn github_sync_correct_args_with_repo() {
        let fixture = make_config(true);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "gh");
        assert_eq!(
            calls[0].1,
            vec!["secret", "set", "MY_KEY", "-R", "owner/repo"]
        );
    }

    #[test]
    fn github_passes_value_via_stdin() {
        let fixture = make_config(true);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "my_secret", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].2.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn github_sync_without_repo() {
        let fixture = make_config(false);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].1, vec!["secret", "set", "KEY"]);
    }

    #[test]
    fn github_sync_with_env_flags() {
        let fixture = make_config(true);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "secret",
                "set",
                "KEY",
                "-R",
                "owner/repo",
                "--env",
                "production"
            ]
        );
    }

    #[test]
    fn github_delete_correct_args() {
        let fixture = make_config(true);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec!["secret", "delete", "MY_KEY", "-R", "owner/repo"]
        );
    }

    #[test]
    fn github_delete_failure() {
        let fixture = make_config(true);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn github_nonzero_exit() {
        let fixture = make_config(false);
        let config = fixture.config();
        let adapter_config = config.adapters.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let adapter = GithubAdapter {
            config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
