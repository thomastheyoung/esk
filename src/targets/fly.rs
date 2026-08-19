//! Fly.io target — deploys secrets via the `fly` CLI.
//!
//! Fly.io is a platform for running full-stack apps close to users on
//! lightweight VMs (Machines). Secrets are encrypted at rest and exposed as
//! environment variables to running applications.
//!
//! CLI: `fly` (Fly.io's official CLI, aka `flyctl`).
//! Commands: `fly secrets import -a <app>` (set) / `fly secrets unset -a <app>` (delete).
//!
//! Secrets are set via **stdin** in `KEY=value` format using `secrets import`.
//! Requires an app name (mapped from esk's app config). Values containing
//! newlines or `=` in keys are rejected since the `KEY=value` stdin format
//! cannot represent them.

use anyhow::{Context, Result};

use std::collections::BTreeSet;

use crate::config::{Config, FlyTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, validate_stdin_kv_value, CommandOpts, CommandRunner,
    DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct FlyTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a FlyTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl FlyTarget<'_> {
    fn resolve_app(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("fly target requires an app")?;
        self.target_config
            .app_names
            .get(app)
            .map(std::string::String::as_str)
            .with_context(|| format!("no fly app_names mapping for '{app}'"))
    }
}

impl DeployTarget for FlyTarget<'_> {
    fn name(&self) -> &'static str {
        "fly"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "fly")
            .context("Install from: https://fly.io/docs/hands-on/install-flyctl/")?;
        let output = self
            .runner
            .run("fly", &["auth", "whoami"], CommandOpts::default())
            .context("failed to run fly auth whoami")?;
        if !output.success {
            anyhow::bail!("fly is not authenticated. Run: fly auth login");
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        validate_stdin_kv_value(key, value, "fly")?;
        let fly_app = self.resolve_app(target)?;
        let stdin_data = format!("{key}={value}\n");

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "import", "-a", fly_app];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "fly",
                &args,
                CommandOpts {
                    stdin: Some(stdin_data.into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run fly secrets import for {key}"))?
            .check("fly secrets import", key)
    }

    /// Presence only. Fly's docs are explicit: "the actual value of the secret
    /// is only available to the application."
    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Presence
    }

    /// List secret names via `fly secrets list --json`.
    ///
    /// Fly returns a **digest** of each value. It is carried in the display-only
    /// `note` and is never compared: esk's own hashes are HMAC-keyed, so
    /// equality with any provider digest is impossible by construction, and
    /// Fly does not document its algorithm in any case. Returning it as
    /// evidence of a value match would be a fabricated verdict — the type
    /// system forbids it here, since `Evidence::Names` can only yield
    /// `PresenceVerdict`.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let fly_app = self.resolve_app(target)?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "list", "-a", fly_app, "--json"];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("fly", &args, CommandOpts::default())
            .context("failed to run fly secrets list")?;

        // A failed listing is an incomplete read, never an empty app.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("fly secrets list failed for {fly_app}: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse fly secrets list JSON response")?;
        let entries = json
            .as_array()
            .context("fly secrets list response was not a JSON array")?;

        let present: std::collections::BTreeSet<String> = entries
            .iter()
            .filter_map(|e| e.get("Name")?.as_str().map(String::from))
            .collect();

        // A secret can be set but not yet live on the machines. That is worth
        // telling the operator, and it is a fact about deployment state rather
        // than about any value, so it belongs in the note.
        let staged = entries
            .iter()
            .filter(|e| {
                e.get("Digest")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(str::is_empty)
            })
            .count();
        let note = (staged > 0)
            .then(|| format!("{staged} secret(s) staged but not yet deployed to machines"));

        Ok(Evidence::Names { present, note })
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let fly_app = self.resolve_app(target)?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "unset", key, "-a", fly_app];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("fly", &args, CommandOpts::default())
            .with_context(|| format!("failed to run fly secrets unset for {key}"))?
            .check("fly secrets unset", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    const FLY_YAML: &str = r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
targets:
  fly:
    app_names:
      web: my-fly-app
    env_flags:
      prod: "--stage"
"#;

    fn make_fixture() -> ConfigFixture {
        ConfigFixture::new(FLY_YAML).unwrap()
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "fly".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn fly_preflight_success() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"user@test".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["auth", "whoami"]);
    }

    #[test]
    fn fly_preflight_auth_failure() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
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
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("fly is not authenticated"));
    }

    #[test]
    fn fly_preflight_missing_cli() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("Install from: https://fly.io"));
    }

    #[test]
    fn fly_deploy_uses_stdin() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "fly");
        assert_eq!(calls[0].args, vec!["secrets", "import", "-a", "my-fly-app"]);
        // Value is passed via stdin, not in args
        assert_eq!(
            calls[0].stdin.as_deref(),
            Some(b"MY_KEY=secret_val\n".as_slice())
        );
        assert!(!calls[0].args.iter().any(|a| a.contains("secret_val")));
    }

    #[test]
    fn fly_deploy_with_env_flags() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["secrets", "import", "-a", "my-fly-app", "--stage"]
        );
        assert_eq!(calls[0].stdin.as_deref(), Some(b"KEY=val\n".as_slice()));
    }

    #[test]
    fn fly_requires_app() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = FlyTarget {
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
    fn fly_unknown_app_mapping() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("api"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("no fly app_names mapping"));
    }

    #[test]
    fn fly_delete_correct_args() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["secrets", "unset", "MY_KEY", "-a", "my-fly-app"]
        );
    }

    #[test]
    fn fly_delete_failure() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = FlyTarget {
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
    fn fly_rejects_newline_in_value() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "line1\nline2", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn fly_rejects_cr_in_value() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "line1\r\nline2", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn fly_nonzero_exit() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"deploy error".to_vec(),
        }]);
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("deploy error"));
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

    fn secrets_list_json(entries: &[(&str, &str)]) -> Vec<u8> {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|(name, digest)| serde_json::json!({ "Name": name, "Digest": digest }))
            .collect();
        serde_json::to_vec(&items).unwrap()
    }

    /// Fly returns a digest of each value. It must never become a verdict:
    /// esk's own hashes are HMAC-keyed, so no provider digest can ever equal
    /// one, and Fly does not document its algorithm. The type system enforces
    /// this — `Evidence::Names` can only produce `PresenceVerdict`.
    #[test]
    fn fly_read_back_never_compares_the_provider_digest() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secrets_list_json(&[("API_KEY", "abc123digest")]), b"");
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "the_real_value")]),
        );
        let crate::verify::Findings::Presence { verdicts, .. } = &findings else {
            panic!("fly declares Fidelity::Presence, so it must yield presence findings");
        };
        assert_eq!(
            verdicts["API_KEY"],
            crate::verify::PresenceVerdict::Present,
            "the key exists; the digest proves nothing about the value"
        );
    }

    /// A staged-but-undeployed secret is a fact about deployment state, not
    /// about a value, so it is surfaced as a display-only note.
    #[test]
    fn fly_read_back_notes_staged_secrets() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &secrets_list_json(&[("API_KEY", ""), ("OTHER", "digest")]),
            b"",
        );
        let target = FlyTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev"))
            .unwrap();
        let Evidence::Names { present, note } = evidence else {
            panic!("expected names");
        };
        assert!(present.contains("API_KEY"));
        let note = note.expect("a staged secret must be surfaced");
        assert!(note.contains("staged"), "got: {note}");
    }

    #[test]
    fn fly_read_back_missing_key_is_drift() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secrets_list_json(&[("SOMETHING_ELSE", "d")]), b"");
        let target = FlyTarget {
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
    }

    #[test]
    fn fly_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_fixture();
        let config = fixture.config();
        let target_config = config.targets.fly.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"Could not find App");
        let target = FlyTarget {
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
}
