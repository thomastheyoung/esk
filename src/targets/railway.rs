//! Railway target — deploys variables via the `railway` CLI.
//!
//! Railway is a cloud platform for deploying applications with managed
//! infrastructure. Variables are scoped to a service/environment and injected
//! at runtime.
//!
//! CLI: `railway` (Railway's official CLI).
//! Commands: `railway variables set KEY --stdin` / `railway variables delete KEY`.
//!
//! Secrets are set via **stdin** using the `--stdin` flag on `railway variables set`.
//! This avoids exposing secret values in process arguments. The CLI determines
//! the target project/service from the linked context (no explicit app flag needed).

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use std::collections::BTreeSet;

use crate::config::{Config, RailwayTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct RailwayTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a RailwayTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for RailwayTarget<'_> {
    fn name(&self) -> &'static str {
        "railway"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "railway")
            .context("Install from: https://docs.railway.app/guides/cli")?;
        let output = self
            .runner
            .run("railway", &["whoami"], CommandOpts::default())
            .context("failed to run railway whoami")?;
        if !output.success {
            anyhow::bail!("railway is not authenticated. Run: railway login");
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variables", "set", key, "--stdin"];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "railway",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run railway variables set for {key}"))?
            .check("railway variables set", key)
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `railway variables --kv`, which prints `KEY=VALUE` lines.
    ///
    /// The read relies on the same linked project the deploy does: esk writes
    /// via `railway variables set` without an explicit service or environment
    /// flag, so the read must not add one either, or the two would address
    /// different scopes.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variables", "--kv"];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("railway", &args, CommandOpts::default())
            .context("failed to run railway variables")?;

        // A failed listing is an incomplete read, never an empty project.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("railway variables failed: {stderr}");
        }

        Ok(Evidence::Values(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| line.trim_end_matches('\r').split_once('='))
                .map(|(key, value)| (key.trim().to_string(), Zeroizing::new(value.to_string())))
                .collect(),
        ))
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variables", "delete", key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("railway", &args, CommandOpts::default())
            .with_context(|| format!("failed to run railway variables delete for {key}"))?
            .check("railway variables delete", key)
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
targets:
  railway:
    env_flags:
      prod: "--environment production"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "railway".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn railway_preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"3.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"user@test".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].args, vec!["whoami"]);
        assert!(calls[1].stdin.is_none());
    }

    #[test]
    fn railway_preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"3.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not logged in".to_vec(),
            },
        ]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("railway is not authenticated"));
    }

    #[test]
    fn railway_preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Install from: https://docs.railway.app"));
    }

    #[test]
    fn railway_deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "railway");
        assert_eq!(calls[0].args, vec!["variables", "set", "MY_KEY", "--stdin"]);
        // Value is passed via stdin, not in args
        assert_eq!(calls[0].stdin.as_deref(), Some(b"secret_val".as_slice()));
        assert!(!calls[0].args.iter().any(|a| a.contains("secret_val")));
    }

    #[test]
    fn railway_deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "variables",
                "set",
                "KEY",
                "--stdin",
                "--environment",
                "production"
            ]
        );
        assert_eq!(calls[0].stdin.as_deref(), Some(b"val".as_slice()));
    }

    #[test]
    fn railway_delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["variables", "delete", "MY_KEY"]);
        assert!(calls[0].stdin.is_none());
    }

    #[test]
    fn railway_delete_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("KEY", &make_target("prod")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["variables", "delete", "KEY", "--environment", "production"]
        );
    }

    #[test]
    fn railway_delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn railway_nonzero_exit() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"api error".to_vec(),
        }]);
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("api error"));
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

    #[test]
    fn railway_read_back_parses_kv_lines() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            b"API_KEY=secret1\nDATABASE_URL=postgres://u:p@h/db?x=1\n",
            b"",
        );
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target("dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("railway declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");
        // Values containing `=` must survive: split on the first one only.
        assert_eq!(values["DATABASE_URL"].as_str(), "postgres://u:p@h/db?x=1");

        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["variables", "--kv"]);
    }

    #[test]
    fn railway_read_back_surfaces_wrong_value_as_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(b"API_KEY=STALE\n", b"");
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "current")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
    }

    #[test]
    fn railway_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.railway.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"No linked project");
        let target = RailwayTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
    }
}
