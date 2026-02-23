use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, CommandOpts, CommandRunner, SyncAdapter,
    SyncMode,
};
use crate::config::{CloudflareAdapterConfig, Config, ResolvedTarget};

pub struct CloudflareAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a CloudflareAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> CloudflareAdapter<'a> {
    fn sync_pages_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let project = self
            .adapter_config
            .pages_project
            .as_deref()
            .context("cloudflare pages_project is required when mode is 'pages'")?;

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["pages", "secret", "put", key, "--project", project];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "wrangler",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run wrangler pages for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("wrangler pages secret put failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_pages_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let project = self
            .adapter_config
            .pages_project
            .as_deref()
            .context("cloudflare pages_project is required when mode is 'pages'")?;

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["pages", "secret", "delete", key, "--project", project, "--force"];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("wrangler", &args, CommandOpts::default())
            .with_context(|| format!("failed to run wrangler pages delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("wrangler pages secret delete failed for {key}: {stderr}");
        }

        Ok(())
    }
}

impl<'a> SyncAdapter for CloudflareAdapter<'a> {
    fn name(&self) -> &str {
        "cloudflare"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "wrangler").map_err(|_| {
            anyhow::anyhow!(
                "wrangler is not installed or not in PATH. Install it with: npm install -g wrangler"
            )
        })?;
        let output = self
            .runner
            .run("wrangler", &["whoami"], CommandOpts::default())
            .context("failed to run wrangler whoami")?;
        if !output.success {
            anyhow::bail!("wrangler is not authenticated. Run: wrangler login");
        }
        Ok(())
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        if self.adapter_config.mode == "pages" {
            return self.sync_pages_secret(key, value, target);
        }

        let app = target
            .app
            .as_deref()
            .context("cloudflare adapter requires an app")?;
        let app_config = self
            .config
            .apps
            .get(app)
            .with_context(|| format!("unknown app '{app}'"))?;
        let app_path = self.config.root.join(&app_config.path);

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "put", key];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "wrangler",
                &args,
                CommandOpts {
                    cwd: Some(app_path),
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run wrangler for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("wrangler secret put failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        if self.adapter_config.mode == "pages" {
            return self.delete_pages_secret(key, target);
        }

        let app = target
            .app
            .as_deref()
            .context("cloudflare adapter requires an app")?;
        let app_config = self
            .config
            .apps
            .get(app)
            .with_context(|| format!("unknown app '{app}'"))?;
        let app_path = self.config.root.join(&app_config.path);

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "delete", key, "--force"];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "wrangler",
                &args,
                CommandOpts {
                    cwd: Some(app_path),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run wrangler delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("wrangler secret delete failed for {key}: {stderr}");
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
                Option<Vec<u8>>,
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
            Option<Vec<u8>>,
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

    fn make_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
adapters:
  cloudflare:
    env_flags:
      prod: "--env production"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            adapter: "cloudflare".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn cloudflare_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"user@example.com".to_vec(),
                stderr: vec![],
            },
        ]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["whoami"]);
    }

    #[test]
    fn cloudflare_preflight_not_authenticated() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![
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
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("wrangler is not authenticated"));
        assert!(err.to_string().contains("wrangler login"));
    }

    #[test]
    fn cloudflare_preflight_missing_wrangler() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();

        // Runner that returns an error (simulating missing command)
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }

        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &FailRunner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("wrangler is not installed"));
        assert!(err.to_string().contains("npm install -g wrangler"));
    }

    #[test]
    fn cloudflare_requires_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![]);
        let adapter = CloudflareAdapter {
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
    fn cloudflare_unknown_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target(Some("nope"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("unknown app 'nope'"));
    }

    #[test]
    fn cloudflare_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target(Some("web"), "prod"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "wrangler");
        assert_eq!(
            calls[0].1,
            vec!["secret", "put", "MY_KEY", "--env", "production"]
        );
        assert_eq!(calls[0].2.as_ref().unwrap(), &dir.path().join("apps/web"));
    }

    #[test]
    fn cloudflare_passes_value_via_stdin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "my_secret", &make_target(Some("web"), "dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls[0].3.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn cloudflare_empty_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap();

        let calls = runner.take_calls();
        // dev has no env_flags, so just: secret put KEY
        assert_eq!(calls[0].1, vec!["secret", "put", "KEY"]);
    }

    #[test]
    fn cloudflare_delete_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target(Some("web"), "prod"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "wrangler");
        assert_eq!(
            calls[0].1,
            vec![
                "secret",
                "delete",
                "MY_KEY",
                "--force",
                "--env",
                "production"
            ]
        );
    }

    #[test]
    fn cloudflare_delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = CloudflareAdapter {
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
    fn cloudflare_delete_requires_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .delete_secret("KEY", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("requires an app"));
    }

    #[test]
    fn cloudflare_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }

    // --- Pages mode tests ---

    fn make_pages_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: x
environments: [dev, prod]
adapters:
  cloudflare:
    mode: pages
    pages_project: my-pages-app
    env_flags:
      prod: "--env production"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    #[test]
    fn pages_sync_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_pages_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target(None, "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].0, "wrangler");
        assert_eq!(
            calls[0].1,
            vec!["pages", "secret", "put", "MY_KEY", "--project", "my-pages-app"]
        );
        assert_eq!(calls[0].3.as_ref().unwrap(), b"secret_val");
    }

    #[test]
    fn pages_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_pages_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target(None, "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].1,
            vec!["pages", "secret", "put", "KEY", "--project", "my-pages-app", "--env", "production"]
        );
    }

    #[test]
    fn pages_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_pages_config(dir.path());
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target(None, "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].1,
            vec!["pages", "secret", "delete", "MY_KEY", "--project", "my-pages-app", "--force"]
        );
    }

    #[test]
    fn pages_missing_project() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
adapters:
  cloudflare:
    mode: pages
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let adapter_config = config.adapters.cloudflare.as_ref().unwrap();
        let runner = MockRunner::new(vec![]);
        let adapter = CloudflareAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("pages_project is required"));
    }
}
