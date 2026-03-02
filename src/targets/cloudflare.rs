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

use crate::config::{CloudflareMode, CloudflareTargetConfig, Config, ResolvedTarget};
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

        self.runner
            .run(
                "wrangler",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run wrangler pages secret put for {key}"))?
            .check("wrangler pages secret put", key)
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

        self.runner
            .run("wrangler", &args, CommandOpts::default())
            .with_context(|| format!("failed to run wrangler pages secret delete for {key}"))?
            .check("wrangler pages secret delete", key)
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
        if self.target_config.mode == CloudflareMode::Pages {
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

        self.runner
            .run(
                "wrangler",
                &args,
                CommandOpts {
                    cwd: Some(app_path),
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run wrangler secret put for {key}"))?
            .check("wrangler secret put", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        if self.target_config.mode == CloudflareMode::Pages {
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

        self.runner
            .run(
                "wrangler",
                &args,
                CommandOpts {
                    cwd: Some(app_path),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run wrangler secret delete for {key}"))?
            .check("wrangler secret delete", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_config() -> ConfigFixture {
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
        ConfigFixture::new(yaml).expect("fixture")
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
        let fixture = make_config();
        let config = fixture.config();
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
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["whoami"]);
    }

    #[test]
    fn cloudflare_preflight_not_authenticated() {
        let fixture = make_config();
        let config = fixture.config();
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
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("wrangler is not authenticated"));
        assert!(err.to_string().contains("wrangler login"));
    }

    #[test]
    fn cloudflare_preflight_missing_wrangler() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();

        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("wrangler is not installed"));
        assert!(err.to_string().contains("npm install -g wrangler"));
    }

    #[test]
    fn cloudflare_requires_app() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = CloudflareTarget {
            config,
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
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = CloudflareTarget {
            config,
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
        let fixture = make_config();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(Some("web"), "prod"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "wrangler");
        assert_eq!(
            calls[0].args,
            vec!["secret", "put", "MY_KEY", "--env", "production"]
        );
        assert_eq!(calls[0].cwd.as_ref().unwrap(), &fixture.path("apps/web"));
    }

    #[test]
    fn cloudflare_passes_value_via_stdin() {
        let fixture = make_config();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "my_secret", &make_target(Some("web"), "dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls[0].stdin.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn cloudflare_empty_env_flags() {
        let fixture = make_config();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap();

        let calls = runner.take_calls();
        // dev has no env_flags, so just: secret put KEY
        assert_eq!(calls[0].args, vec!["secret", "put", "KEY"]);
    }

    #[test]
    fn cloudflare_delete_builds_correct_command() {
        let fixture = make_config();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(Some("web"), "prod"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "wrangler");
        assert_eq!(
            calls[0].args,
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
        let fixture = make_config();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = CloudflareTarget {
            config,
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
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = CloudflareTarget {
            config,
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
        let fixture = make_config();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }

    // --- Pages mode tests ---

    fn make_pages_config() -> ConfigFixture {
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
        ConfigFixture::new(yaml).expect("fixture")
    }

    #[test]
    fn pages_deploy_correct_args() {
        let fixture = make_pages_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(None, "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "wrangler");
        assert_eq!(
            calls[0].args,
            vec![
                "pages",
                "secret",
                "put",
                "MY_KEY",
                "--project",
                "my-pages-app"
            ]
        );
        assert_eq!(calls[0].stdin.as_ref().unwrap(), b"secret_val");
    }

    #[test]
    fn pages_deploy_with_env_flags() {
        let fixture = make_pages_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(None, "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
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
        let fixture = make_pages_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(None, "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
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
