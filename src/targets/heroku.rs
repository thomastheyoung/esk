//! Heroku target — deploys config vars via the `heroku` CLI.
//!
//! Heroku is a cloud PaaS that runs applications in managed containers (dynos).
//! Config vars are exposed as environment variables to the running application
//! and persist across deploys.
//!
//! CLI: `heroku` (Heroku's official CLI).
//! Commands: `heroku config:set KEY=value -a <app>` / `heroku config:unset KEY -a <app>`.
//!
//! The Heroku CLI does **not** support stdin for secret values, so they are
//! passed as command-line arguments (visible in `ps` output). Requires an app
//! name (mapped from esk's app config).

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use std::collections::BTreeSet;

use crate::config::{Config, HerokuTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct HerokuTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a HerokuTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl HerokuTarget<'_> {
    fn resolve_app(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("heroku target requires an app")?;
        self.target_config
            .app_names
            .get(app)
            .map(std::string::String::as_str)
            .with_context(|| format!("no heroku app_names mapping for '{app}'"))
    }
}

impl DeployTarget for HerokuTarget<'_> {
    fn name(&self) -> &'static str {
        "heroku"
    }

    fn passes_value_as_cli_arg(&self) -> bool {
        true
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "heroku")
            .context("Install from: https://devcenter.heroku.com/articles/heroku-cli")?;
        let output = self
            .runner
            .run("heroku", &["auth:whoami"], CommandOpts::default())
            .context("failed to run heroku auth:whoami")?;
        if !output.success {
            anyhow::bail!("heroku is not authenticated. Run: heroku login");
        }
        Ok(())
    }

    // SECURITY: heroku CLI has no stdin/file support for config:set. Secret values are exposed
    // in process arguments (visible via `ps aux`). Feature requested upstream since 2016, never
    // implemented. No workaround available.
    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let heroku_app = self.resolve_app(target)?;
        let kv = format!("{key}={value}");

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["config:set", &kv, "-a", heroku_app];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("heroku", &args, CommandOpts::default())
            .with_context(|| format!("failed to run heroku config:set for {key}"))?
            .check("heroku config:set", key)
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `heroku config -a <app> --json`.
    ///
    /// A new call rather than a reused one — unlike convex or aws_lambda,
    /// nothing on heroku's deploy path already reads the config. `--json` is
    /// used so values containing spaces or `=` survive parsing, which the
    /// default table output does not guarantee.
    ///
    /// `keys` is unused: heroku returns the app's whole config in one call and
    /// offers no per-key read.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let heroku_app = self.resolve_app(target)?;
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["config", "-a", heroku_app, "--json"];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("heroku", &args, CommandOpts::default())
            .with_context(|| format!("failed to run heroku config for {heroku_app}"))?;

        // A failed read is an incomplete read, never an empty app: returning
        // an empty map would report every managed key as missing.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("heroku config failed for {heroku_app}: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse heroku config JSON response")?;
        let object = json
            .as_object()
            .context("heroku config response was not a JSON object")?;

        Ok(Evidence::Values(
            object
                .iter()
                // Non-string values are not config vars esk can compare. They
                // are skipped rather than stringified, since a coerced value
                // would mismatch and read as drift on a key esk does not set.
                .filter_map(|(key, value)| {
                    value
                        .as_str()
                        .map(|v| (key.clone(), Zeroizing::new(v.to_string())))
                })
                .collect(),
        ))
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let heroku_app = self.resolve_app(target)?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["config:unset", key, "-a", heroku_app];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("heroku", &args, CommandOpts::default())
            .with_context(|| format!("failed to run heroku config:unset for {key}"))?
            .check("heroku config:unset", key)
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
  heroku:
    app_names:
      web: my-heroku-app
    env_flags:
      prod: "--remote staging"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "heroku".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn heroku_preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
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
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].args, vec!["auth:whoami"]);
    }

    #[test]
    fn heroku_preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
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
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("heroku is not authenticated"));
    }

    #[test]
    fn heroku_preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Install from: https://devcenter.heroku.com"));
    }

    #[test]
    fn heroku_deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "heroku");
        assert_eq!(
            calls[0].args,
            vec!["config:set", "MY_KEY=secret_val", "-a", "my-heroku-app"]
        );
    }

    #[test]
    fn heroku_deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = HerokuTarget {
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
            vec![
                "config:set",
                "KEY=val",
                "-a",
                "my-heroku-app",
                "--remote",
                "staging"
            ]
        );
    }

    #[test]
    fn heroku_requires_app() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = HerokuTarget {
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
    fn heroku_unknown_app_mapping() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("api"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("no heroku app_names mapping"));
    }

    #[test]
    fn heroku_delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = HerokuTarget {
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
            vec!["config:unset", "MY_KEY", "-a", "my-heroku-app"]
        );
    }

    #[test]
    fn heroku_delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = HerokuTarget {
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
    fn heroku_nonzero_exit() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }

    fn verify_keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn verify_expected(
        pairs: &[(&str, &str)],
    ) -> std::collections::BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    fn config_json(pairs: &[(&str, &str)]) -> Vec<u8> {
        let map: serde_json::Map<String, serde_json::Value> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::json!(v)))
            .collect();
        serde_json::to_vec(&map).unwrap()
    }

    #[test]
    fn heroku_read_back_returns_config_vars() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&config_json(&[("API_KEY", "secret1")]), b"");
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("heroku declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "heroku");
        assert_eq!(
            calls[0].args,
            vec!["config", "-a", "my-heroku-app", "--json"]
        );
    }

    /// The negative case: a happy-path test cannot distinguish a working
    /// reader from one that echoes the store back.
    #[test]
    fn heroku_read_back_surfaces_wrong_value_as_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&config_json(&[("API_KEY", "STALE")]), b"");
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "current")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Differs);
    }

    #[test]
    fn heroku_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"Couldn't find that app");
        let target = HerokuTarget {
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
        assert!(matches!(
            findings,
            crate::verify::Findings::Unreachable { .. }
        ));
    }

    #[test]
    fn heroku_read_back_preserves_values_containing_equals_and_spaces() {
        // The reason for `--json`: the default table output cannot be parsed
        // unambiguously when values contain the separator or whitespace.
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &config_json(&[("URL", "postgres://u:p@h/db?a=1"), ("MSG", "hello world")]),
            b"",
        );
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(
                &verify_keys(&["URL", "MSG"]),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values");
        };
        assert_eq!(values["URL"].as_str(), "postgres://u:p@h/db?a=1");
        assert_eq!(values["MSG"].as_str(), "hello world");
    }

    #[test]
    fn heroku_read_back_applies_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&config_json(&[("A", "1")]), b"");
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .read_back(&verify_keys(&["A"]), &make_target(Some("web"), "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--remote".to_string()));
    }

    #[test]
    fn heroku_read_back_requires_an_app() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.heroku.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        let target = HerokuTarget {
            config,
            target_config,
            runner: &runner,
        };
        let Err(err) = target.read_back(&verify_keys(&["A"]), &make_target(None, "dev")) else {
            panic!("no app means no config to read; it must not report an empty read");
        };
        assert!(err.to_string().contains("requires an app"));
    }
}
