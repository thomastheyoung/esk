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

use std::collections::BTreeSet;

use crate::config::{CloudflareMode, CloudflareTargetConfig, Config, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

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
        check_command(self.runner, "wrangler").context("Install with: npm install -g wrangler")?;
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

    /// Presence, permanently. `wrangler secret list` returns each secret's
    /// name and type and nothing else — no value, and no digest either.
    /// Cloudflare cannot tell esk whether a stored value is correct.
    fn verify_fidelity(&self) -> Fidelity {
        match self.target_config.mode {
            CloudflareMode::Pages => Fidelity::None,
            CloudflareMode::Workers => Fidelity::Presence,
        }
    }

    /// List secret names via `wrangler secret list`.
    ///
    /// Returns [`Evidence::Names`], so [`compare`](crate::verify::compare) can
    /// only produce [`PresenceVerdict`](crate::verify::PresenceVerdict) — a
    /// type with no "matches" variant. This target cannot claim a value is
    /// correct even if its implementation were wrong.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        // Pages secrets have no list command; only Workers do.
        if self.target_config.mode == CloudflareMode::Pages {
            return Ok(Evidence::Unreadable(
                "wrangler has no secret list command for Pages projects",
            ));
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
        let mut args: Vec<&str> = vec!["secret", "list", "--format", "json"];
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
            .context("failed to run wrangler secret list")?;

        // A failed listing is an incomplete read, never an empty Worker.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("wrangler secret list failed: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse wrangler secret list JSON response")?;
        let entries = json
            .as_array()
            .context("wrangler secret list response was not a JSON array")?;

        Ok(Evidence::Names {
            present: entries
                .iter()
                .filter_map(|e| e.get("name")?.as_str().map(String::from))
                .collect(),
            // Nothing to display: the listing carries no digest or partial
            // value that could tell an operator anything more.
            note: None,
        })
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
        assert!(err
            .to_string()
            .contains("Install with: npm install -g wrangler"));
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

    fn verify_keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn verify_expected(
        pairs: &[(&str, &str)],
    ) -> std::collections::BTreeMap<String, zeroize::Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), zeroize::Zeroizing::new((*v).to_string())))
            .collect()
    }

    fn secret_list_json(names: &[&str]) -> Vec<u8> {
        let items: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!({ "name": n, "type": "secret_text" }))
            .collect();
        serde_json::to_vec(&items).unwrap()
    }

    #[test]
    fn cloudflare_read_back_lists_secret_names() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["API_KEY", "DB_URL"]), b"");
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev"))
            .unwrap();
        let Evidence::Names { present, note } = evidence else {
            panic!("cloudflare declares Fidelity::Presence, so it must return Names");
        };
        assert!(present.contains("API_KEY"));
        assert!(present.contains("DB_URL"));
        // wrangler returns no digest or partial value, so there is nothing to
        // show an operator beyond the names themselves.
        assert!(note.is_none());

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "wrangler");
        assert_eq!(calls[0].args, vec!["secret", "list", "--format", "json"]);
    }

    /// The structural guarantee: a presence target cannot report that a value
    /// matched, because `PresenceVerdict` has no such variant. Even when the
    /// store's value is wrong, the strongest claim available is `Present`.
    #[test]
    fn cloudflare_read_back_cannot_claim_a_value_matched() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["API_KEY"]), b"");
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "whatever_the_store_holds")]),
        );
        let crate::verify::Findings::Presence { verdicts, .. } = &findings else {
            panic!("expected presence findings");
        };
        assert_eq!(
            verdicts["API_KEY"],
            crate::verify::PresenceVerdict::Present,
            "the key exists; its value was never checked and cannot be claimed"
        );
    }

    /// The negative case for a presence target: a key esk deployed that is no
    /// longer on the Worker.
    #[test]
    fn cloudflare_read_back_missing_key_is_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["SOMETHING_ELSE"]), b"");
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
        let crate::verify::Findings::Presence {
            verdicts, extra, ..
        } = &findings
        else {
            panic!("expected presence findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::PresenceVerdict::Missing);
        assert_eq!(extra, &["SOMETHING_ELSE".to_string()]);
    }

    #[test]
    fn cloudflare_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"Authentication error");
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
    }

    /// Pages has no secret-list command, so it is the one target whose
    /// fidelity is conditional. Pinned so a refactor cannot silently let Pages
    /// claim presence it cannot demonstrate.
    #[test]
    fn cloudflare_pages_mode_is_unverifiable() {
        let yaml = r"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
targets:
  cloudflare:
    mode: pages
    project_names:
      web: my-pages-project
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let config = fixture.config();
        let target_config = config.targets.cloudflare.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        let target = CloudflareTarget {
            config,
            target_config,
            runner: &runner,
        };

        assert_eq!(target.verify_fidelity(), Fidelity::None);
        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert!(matches!(
            findings,
            crate::verify::Findings::Unverifiable { .. }
        ));
        // Nothing was run: there is no command to run.
        assert!(runner.take_calls().is_empty());
    }
}
