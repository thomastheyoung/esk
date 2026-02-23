use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::adapters::{check_command, CommandOpts, CommandRunner, SyncAdapter, SyncMode};
use crate::config::{Config, ConvexAdapterConfig, ResolvedTarget};

pub struct ConvexAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a ConvexAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> ConvexAdapter<'a> {
    /// Resolve the cwd and env vars needed for convex commands.
    fn resolve_deployment_context(&self) -> Result<(PathBuf, Vec<(String, String)>)> {
        let cwd = self.config.root.join(&self.adapter_config.path);
        let mut env_vars: Vec<(String, String)> = Vec::new();

        if let Some(source) = &self.adapter_config.deployment_source {
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

impl<'a> SyncAdapter for ConvexAdapter<'a> {
    fn name(&self) -> &str {
        "convex"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "npx").map_err(|_| {
            anyhow::anyhow!(
                "npx is not installed or not in PATH. Install Node.js to get npx."
            )
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

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let (cwd, env_vars) = self.resolve_deployment_context()?;

        let env_flags = self
            .adapter_config
            .env_flags
            .get(&target.environment)
            .cloned()
            .unwrap_or_default();

        let mut args: Vec<&str> = vec!["convex", "env", "set", key, value];
        let flag_parts: Vec<String>;
        if !env_flags.is_empty() {
            flag_parts = env_flags.split_whitespace().map(String::from).collect();
            for part in &flag_parts {
                args.push(part);
            }
        }

        let output = self
            .runner
            .run(
                "npx",
                &args,
                CommandOpts {
                    cwd: Some(cwd),
                    env: env_vars,
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run convex for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("convex env set failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let (cwd, env_vars) = self.resolve_deployment_context()?;

        let env_flags = self
            .adapter_config
            .env_flags
            .get(&target.environment)
            .cloned()
            .unwrap_or_default();

        let mut args: Vec<&str> = vec!["convex", "env", "unset", key];
        let flag_parts: Vec<String>;
        if !env_flags.is_empty() {
            flag_parts = env_flags.split_whitespace().map(String::from).collect();
            for part in &flag_parts {
                args.push(part);
            }
        }

        let output = self
            .runner
            .run(
                "npx",
                &args,
                CommandOpts {
                    cwd: Some(cwd),
                    env: env_vars,
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run convex delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("convex env unset failed for {key}: {stderr}");
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
        calls: Mutex<
            Vec<(
                String,
                Vec<String>,
                Option<std::path::PathBuf>,
                Vec<(String, String)>,
            )>,
        >,
        responses: Mutex<Vec<CommandOutput>>,
    }

    impl MockRunner {
        fn new(responses: Vec<CommandOutput>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses: Mutex::new(responses),
            }
        }
        fn take_calls(
            &self,
        ) -> Vec<(
            String,
            Vec<String>,
            Option<std::path::PathBuf>,
            Vec<(String, String)>,
        )> {
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
                opts.cwd,
                opts.env,
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

    fn make_config(dir: &std::path::Path, deployment_source: Option<&str>) -> Config {
        let mut yaml = String::from(
            r#"
project: x
environments: [dev, prod]
adapters:
  convex:
    path: apps/api
"#,
        );
        if let Some(s) = deployment_source {
            yaml.push_str(&format!("    deployment_source: {s}\n"));
        }
        yaml.push_str("    env_flags:\n      prod: \"--prod\"\n");
        let path = dir.join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            adapter: "convex".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn convex_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["convex", "env", "list"]);
    }

    #[test]
    fn convex_preflight_deployment_inaccessible() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("convex deployment not accessible"));
        assert!(err.to_string().contains("deployment not found"));
    }

    #[test]
    fn convex_preflight_missing_npx() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path(), None);
        let adapter_config = config.adapters.convex.as_ref().unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }

        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &FailRunner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("npx is not installed"));
        assert!(err.to_string().contains("Node.js"));
    }

    #[test]
    fn convex_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "my_value", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls[0].0, "npx");
        assert_eq!(
            calls[0].1,
            vec!["convex", "env", "set", "MY_KEY", "my_value"]
        );
        assert_eq!(calls[0].2.as_ref().unwrap(), &dir.path().join("apps/api"));
    }

    #[test]
    fn convex_reads_deployment_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let source = dir.path().join("apps/api/.env.local");
        std::fs::write(&source, "CONVEX_DEPLOYMENT=dev:my-deploy-123\n").unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].3.contains(&(
            "CONVEX_DEPLOYMENT".to_string(),
            "dev:my-deploy-123".to_string()
        )));
    }

    #[test]
    fn convex_deployment_source_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].3.is_empty()); // no env vars set
    }

    #[test]
    fn convex_deployment_source_no_match() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let source = dir.path().join("apps/api/.env.local");
        std::fs::write(&source, "OTHER_VAR=foo\nSOMETHING=bar\n").unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].3.is_empty());
    }

    #[test]
    fn convex_deployment_strips_quotes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let source = dir.path().join("apps/api/.env.local");
        std::fs::write(&source, "CONVEX_DEPLOYMENT=\"my-deploy\"\n").unwrap();
        let config = make_config(dir.path(), Some("apps/api/.env.local"));
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0]
            .3
            .contains(&("CONVEX_DEPLOYMENT".to_string(), "my-deploy".to_string())));
    }

    #[test]
    fn convex_delete_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target("prod"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "npx");
        assert_eq!(
            calls[0].1,
            vec!["convex", "env", "unset", "MY_KEY", "--prod"]
        );
    }

    #[test]
    fn convex_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = ConvexAdapter {
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
    fn convex_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/api")).unwrap();
        let config = make_config(dir.path(), None);
        let adapter_config = config.adapters.convex.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"deploy error".to_vec(),
        }]);
        let adapter = ConvexAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("deploy error"));
    }
}
