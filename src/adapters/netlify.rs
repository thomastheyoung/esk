use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SyncAdapter,
    SyncMode,
};
use crate::config::{Config, NetlifyAdapterConfig, ResolvedTarget};

pub struct NetlifyAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a NetlifyAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> SyncAdapter for NetlifyAdapter<'a> {
    fn name(&self) -> &str {
        "netlify"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
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

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:set", key, value];
        if let Some(site) = &self.adapter_config.site {
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
        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:unset", key];
        if let Some(site) = &self.adapter_config.site {
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

    fn make_config(dir: &std::path::Path, with_site: bool) -> Config {
        let yaml = if with_site {
            r#"
project: x
environments: [dev, prod]
adapters:
  netlify:
    site: my-site-id
    env_flags:
      prod: "--context production"
"#
        } else {
            r#"
project: x
environments: [dev, prod]
adapters:
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
            adapter: "netlify".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn netlify_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = NetlifyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].1, vec!["status"]);
    }

    #[test]
    fn netlify_preflight_not_linked() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = NetlifyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("netlify is not linked"));
    }

    #[test]
    fn netlify_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let adapter = NetlifyAdapter {
            config: &config,
            adapter_config,
            runner: &FailRunner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("netlify is not installed"));
    }

    #[test]
    fn netlify_sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = NetlifyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].0, "netlify");
        assert_eq!(calls[0].1, vec!["env:set", "MY_KEY", "secret_val"]);
    }

    #[test]
    fn netlify_sync_with_site() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = NetlifyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].1,
            vec!["env:set", "KEY", "val", "--site", "my-site-id"]
        );
    }

    #[test]
    fn netlify_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = NetlifyAdapter {
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
            vec!["env:set", "KEY", "val", "--context", "production"]
        );
    }

    #[test]
    fn netlify_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), true);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = NetlifyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].1,
            vec!["env:unset", "MY_KEY", "--site", "my-site-id"]
        );
    }

    #[test]
    fn netlify_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = NetlifyAdapter {
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
    fn netlify_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), false);
        let adapter_config = config.adapters.netlify.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let adapter = NetlifyAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }
}
