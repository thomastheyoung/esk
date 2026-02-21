use anyhow::{Context, Result};

use crate::adapters::{CommandOpts, CommandRunner, SyncAdapter, SyncMode};
use crate::config::{CloudflareAdapterConfig, Config, ResolvedTarget};

pub struct CloudflareAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a CloudflareAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> SyncAdapter for CloudflareAdapter<'a> {
    fn name(&self) -> &str {
        "cloudflare"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
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

        let env_flags = self
            .adapter_config
            .env_flags
            .get(&target.environment)
            .cloned()
            .unwrap_or_default();

        let mut args: Vec<&str> = vec!["secret", "put", key];
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
        let path = dir.join("lockbox.yaml");
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
}
