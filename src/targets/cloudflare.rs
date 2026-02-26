//! Cloudflare Pages target — deploys secrets via the `wrangler` CLI.
//!
//! Cloudflare Pages is a Jamstack hosting platform. Each Pages project has its
//! own set of encrypted environment variables (called "secrets") that are
//! injected at build time and into Functions.
//!
//! CLI: `wrangler` (Cloudflare's official CLI, installed via npm).
//! Commands: `wrangler pages secret put` / `wrangler pages secret delete`.
//!
//! Secrets are sent via **stdin** to avoid exposing values in process argument
//! lists. Requires a `--project` flag to identify the Pages project.

use anyhow::{Context, Result};

use crate::config::{CloudflareTargetConfig, Config, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct CloudflareTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a CloudflareTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl CloudflareTarget<'_> {
    fn deploy_pages_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let project = self
            .target_config
            .pages_project
            .as_deref()
            .context("cloudflare pages_project is required when mode is 'pages'")?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["pages", "secret", "put", key, "--project", project];
        args.extend(flag_parts.iter().map(String::as_str));

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
        output.check("wrangler pages secret put", key)?;

        Ok(())
    }

    fn delete_pages_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let project = self
            .target_config
            .pages_project
            .as_deref()
            .context("cloudflare pages_project is required when mode is 'pages'")?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "pages",
            "secret",
            "delete",
            key,
            "--project",
            project,
            "--force",
        ];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("wrangler", &args, CommandOpts::default())
            .with_context(|| format!("failed to run wrangler pages delete for {key}"))?;
        output.check("wrangler pages secret delete", key)?;

        Ok(())
    }
}

impl DeployTarget for CloudflareTarget<'_> {
    fn name(&self) -> &'static str {
        "cloudflare"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
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

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        if self.target_config.mode == "pages" {
            return self.deploy_pages_secret(key, value, target);
        }

        let app = target
            .app
            .as_deref()
            .context("cloudflare target requires an app")?;
        let app_config = self
            .config
            .apps
            .get(app)
            .with_context(|| format!("unknown app '{app}'"))?;
        let app_path = self.config.root.join(&app_config.path);

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "put", key];
        args.extend(flag_parts.iter().map(String::as_str));

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
        output.check("wrangler secret put", key)?;

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        if self.target_config.mode == "pages" {
            return self.delete_pages_secret(key, target);
        }

        let app = target
            .app
            .as_deref()
            .context("cloudflare target requires an app")?;
        let app_config = self
            .config
            .apps
            .get(app)
            .with_context(|| format!("unknown app '{app}'"))?;
        let app_path = self.config.root.join(&app_config.path);

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "delete", key, "--force"];
        args.extend(flag_parts.iter().map(String::as_str));

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
        output.check("wrangler secret delete", key)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (
        String,
        Vec<String>,
        Option<std::path::PathBuf>,
        Option<Vec<u8>>,
    );

    fn take_calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .take_calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.cwd, call.stdin))
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
            service: "cloudflare".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn cloudflare_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
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
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["whoami"]);
    }

    #[test]
    fn cloudflare_preflight_not_authenticated() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
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
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("wrangler is not authenticated"));
        assert!(err.to_string().contains("wrangler login"));
    }

    #[test]
    fn cloudflare_preflight_missing_wrangler() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();

        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("wrangler is not installed"));
        assert!(err.to_string().contains("npm install -g wrangler"));
    }

    #[test]
    fn cloudflare_requires_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("requires an app"));
    }

    #[test]
    fn cloudflare_unknown_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("nope"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("unknown app 'nope'"));
    }

    #[test]
    fn cloudflare_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(Some("web"), "prod"))
            .unwrap();

        let calls = take_calls(&runner);
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
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "my_secret", &make_target(Some("web"), "dev"))
            .unwrap();

        let calls = take_calls(&runner);
        assert_eq!(calls[0].3.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn cloudflare_empty_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap();

        let calls = take_calls(&runner);
        // dev has no env_flags, so just: secret put KEY
        assert_eq!(calls[0].1, vec!["secret", "put", "KEY"]);
    }

    #[test]
    fn cloudflare_delete_builds_correct_command() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(Some("web"), "prod"))
            .unwrap();

        let calls = take_calls(&runner);
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
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = CloudflareTarget {
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
    fn cloudflare_delete_requires_app() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("requires an app"));
    }

    #[test]
    fn cloudflare_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }

    // --- Pages mode tests ---

    fn make_pages_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: x
environments: [dev, prod]
targets:
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
    fn pages_deploy_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_pages_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(None, "dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "wrangler");
        assert_eq!(
            calls[0].1,
            vec![
                "pages",
                "secret",
                "put",
                "MY_KEY",
                "--project",
                "my-pages-app"
            ]
        );
        assert_eq!(calls[0].3.as_ref().unwrap(), b"secret_val");
    }

    #[test]
    fn pages_deploy_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_pages_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(None, "prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "pages",
                "secret",
                "put",
                "KEY",
                "--project",
                "my-pages-app",
                "--env",
                "production"
            ]
        );
    }

    #[test]
    fn pages_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_pages_config(dir.path());
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(None, "dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "pages",
                "secret",
                "delete",
                "MY_KEY",
                "--project",
                "my-pages-app",
                "--force"
            ]
        );
    }

    #[test]
    fn pages_missing_project() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: x
environments: [dev]
targets:
  cloudflare:
    mode: pages
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = CloudflareTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("pages_project is required"));
    }
}
