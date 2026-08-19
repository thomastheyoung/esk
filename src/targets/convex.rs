//! Convex target — deploys environment variables via `npx convex`.
//!
//! Convex is a backend-as-a-service platform with a real-time database and
//! serverless functions. Environment variables are set per-deployment and
//! are available to Convex functions at runtime.
//!
//! CLI: `npx convex` (runs via npx, no global install needed).
//! Commands: `convex env set` / `convex env unset`.
//!
//! Secrets are set via **stdin** — when `convex env set NAME` receives piped
//! input (non-TTY stdin), it reads the value from stdin. This avoids exposing
//! secret values in process arguments. The `CONVEX_DEPLOYMENT` environment
//! variable is read from the project's Convex config file and injected into
//! the command environment.

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::config::{Config, ConvexTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct ConvexTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a ConvexTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl ConvexTarget<'_> {
    /// Resolve the cwd and env vars needed for convex commands.
    fn resolve_deployment_context(&self) -> Result<(PathBuf, Vec<(String, String)>)> {
        let cwd = self.config.root.join(&self.target_config.path);
        let mut env_vars: Vec<(String, String)> = Vec::new();

        if let Some(source) = &self.target_config.deployment_source {
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

/// Parse the `KEY=VALUE` lines `convex env list` prints.
///
/// A value may itself contain `=`, so the split is on the first one only.
/// Lines without `=` are not variables — `convex` prints status text on
/// stderr, but a future version printing a header to stdout must not become a
/// phantom key that reports as drift.
fn parse_env_list(stdout: &[u8]) -> BTreeMap<String, Zeroizing<String>> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            (
                key.trim().to_string(),
                Zeroizing::new(value.trim_end_matches('\r').to_string()),
            )
        })
        .collect()
}

impl DeployTarget for ConvexTarget<'_> {
    fn name(&self) -> &'static str {
        "convex"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "npx")
            .context("Install Node.js to get npx")?;
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

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let (cwd, env_vars) = self.resolve_deployment_context()?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["convex", "env", "set", key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "npx",
                &args,
                CommandOpts {
                    cwd: Some(cwd),
                    env: env_vars,
                    stdin: Some(value.as_bytes().to_vec()),
                },
            )
            .with_context(|| format!("failed to run convex env set for {key}"))?
            .check("convex env set", key)
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `convex env list`, the same command preflight already runs.
    ///
    /// `keys` is unused because the listing is unconditional: convex has no
    /// per-key read, and asking for the whole map costs one call rather than
    /// one per secret. [`compare`](crate::verify::compare) narrows the result
    /// to the managed keys.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let (cwd, env_vars) = self.resolve_deployment_context()?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["convex", "env", "list"];
        args.extend(flag_parts.iter().map(String::as_str));

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
            .context("failed to run convex env list")?;

        // A failed listing is an incomplete read, never an empty target: an
        // empty map here would be reported as every managed key missing.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("convex env list failed: {stderr}");
        }

        Ok(Evidence::Values(parse_env_list(&output.stdout)))
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let (cwd, env_vars) = self.resolve_deployment_context()?;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["convex", "env", "unset", key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "npx",
                &args,
                CommandOpts {
                    cwd: Some(cwd),
                    env: env_vars,
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run convex env unset for {key}"))?
            .check("convex env unset", key)
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;

    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_fixture(deployment_source: Option<&str>) -> ConfigFixture {
        let mut yaml = String::from(
            r"
project: x
environments: [dev, prod]
targets:
  convex:
    path: apps/api
",
        );
        if let Some(s) = deployment_source {
            let _ = writeln!(yaml, "    deployment_source: {s}");
        }
        yaml.push_str("    env_flags:\n      prod: \"--prod\"\n");
        ConfigFixture::new(&yaml).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "convex".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn convex_preflight_success() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
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
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["convex", "env", "list"]);
    }

    #[test]
    fn convex_preflight_deployment_inaccessible() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
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
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("convex deployment not accessible"));
        assert!(err.to_string().contains("deployment not found"));
    }

    #[test]
    fn convex_preflight_missing_npx() {
        let fixture = make_fixture(None);
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();

        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("Install Node.js to get npx"));
    }

    #[test]
    fn convex_builds_correct_command() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "my_value", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "npx");
        assert_eq!(calls[0].args, vec!["convex", "env", "set", "MY_KEY"]);
        assert_eq!(
            calls[0].cwd.as_ref().unwrap(),
            &fixture.path("apps/api")
        );
        // Value is passed via stdin, not in args
        assert_eq!(calls[0].stdin.as_deref(), Some(b"my_value".as_slice()));
        assert!(!calls[0].args.iter().any(|a| a.contains("my_value")));
    }

    #[test]
    fn convex_reads_deployment_source() {
        let fixture = make_fixture(Some("apps/api/.env.local"));
        fixture.create_dir_all("apps/api").unwrap();
        std::fs::write(
            fixture.path("apps/api/.env.local"),
            "CONVEX_DEPLOYMENT=dev:my-deploy-123\n",
        )
        .unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].env.contains(&(
            "CONVEX_DEPLOYMENT".to_string(),
            "dev:my-deploy-123".to_string()
        )));
    }

    #[test]
    fn convex_deployment_source_missing_file() {
        let fixture = make_fixture(Some("apps/api/.env.local"));
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].env.is_empty()); // no env vars set
    }

    #[test]
    fn convex_deployment_source_no_match() {
        let fixture = make_fixture(Some("apps/api/.env.local"));
        fixture.create_dir_all("apps/api").unwrap();
        std::fs::write(
            fixture.path("apps/api/.env.local"),
            "OTHER_VAR=foo\nSOMETHING=bar\n",
        )
        .unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0].env.is_empty());
    }

    #[test]
    fn convex_deployment_strips_quotes() {
        let fixture = make_fixture(Some("apps/api/.env.local"));
        fixture.create_dir_all("apps/api").unwrap();
        std::fs::write(
            fixture.path("apps/api/.env.local"),
            "CONVEX_DEPLOYMENT=\"my-deploy\"\n",
        )
        .unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert!(calls[0]
            .env
            .contains(&("CONVEX_DEPLOYMENT".to_string(), "my-deploy".to_string())));
    }

    #[test]
    fn convex_delete_builds_correct_command() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target("prod"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "npx");
        assert_eq!(
            calls[0].args,
            vec!["convex", "env", "unset", "MY_KEY", "--prod"]
        );
    }

    #[test]
    fn convex_delete_failure() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = ConvexTarget {
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
    fn convex_nonzero_exit() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"deploy error".to_vec(),
        }]);
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("deploy error"));
    }

    /// Helper: the key-name set a verifier is handed. Values never appear here.
    fn keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn expected(pairs: &[(&str, &str)]) -> BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    fn read_back_runner(stdout: &[u8]) -> MockCommandRunner {
        let runner = MockCommandRunner::new().strict();
        runner.push_success(stdout, b"");
        runner
    }

    #[test]
    fn convex_read_back_returns_listed_values() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = read_back_runner(b"API_KEY=secret1\nDB_URL=postgres://x\n");
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&keys(&["API_KEY", "DB_URL"]), &make_target("dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("convex declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");
        assert_eq!(values["DB_URL"].as_str(), "postgres://x");

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "npx");
        assert_eq!(calls[0].args, vec!["convex", "env", "list"]);
    }

    #[test]
    fn convex_read_back_surfaces_wrong_value_as_drift() {
        // The negative case: the provider holds a stale value. A happy-path
        // test cannot distinguish a working verifier from one that echoes back
        // whatever it was asked about, so this is the test that has teeth.
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = read_back_runner(b"API_KEY=STALE_VALUE\n");
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };

        let want = expected(&[("API_KEY", "current_value")]);
        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&keys(&["API_KEY"]), &make_target("dev")),
            &want,
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
    fn convex_read_back_missing_key_is_not_a_match() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = read_back_runner(b"OTHER=x\n");
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };

        let want = expected(&[("API_KEY", "v")]);
        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&keys(&["API_KEY"]), &make_target("dev")),
            &want,
        );
        let crate::verify::Findings::Values { verdicts, extra } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Missing);
        assert_eq!(extra, &["OTHER".to_string()]);
    }

    #[test]
    fn convex_read_back_failure_is_unreachable_not_empty() {
        // A failed listing must never degrade into "the target holds nothing",
        // which would report every managed key as missing — drift the operator
        // would chase instead of the real problem, an unreadable deployment.
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"deployment not found");
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };

        let want = expected(&[("API_KEY", "v")]);
        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&keys(&["API_KEY"]), &make_target("dev")),
            &want,
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
        assert!(matches!(
            findings,
            crate::verify::Findings::Unreachable { .. }
        ));
    }

    #[test]
    fn convex_read_back_error_does_not_echo_secret_values() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"rejected value hunter2");
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };

        let want = expected(&[("API_KEY", "hunter2")]);
        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&keys(&["API_KEY"]), &make_target("dev")),
            &want,
        );
        let crate::verify::Findings::Unreachable { error } = &findings else {
            panic!("expected unreachable findings");
        };
        assert!(!error.contains("hunter2"), "provider error leaked a secret: {error}");
        assert!(error.contains("<redacted>"));
    }

    #[test]
    fn convex_read_back_splits_on_first_equals_only() {
        // Values legitimately contain '='; base64 padding and connection
        // strings both do. Splitting on the last one would corrupt them.
        let parsed = parse_env_list(b"URL=postgres://u:p@h/db?x=1\nPAD=YWJj==\n");
        assert_eq!(parsed["URL"].as_str(), "postgres://u:p@h/db?x=1");
        assert_eq!(parsed["PAD"].as_str(), "YWJj==");
    }

    #[test]
    fn convex_read_back_ignores_lines_without_a_separator() {
        let parsed = parse_env_list(b"Environment variables:\nA=1\n");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed["A"].as_str(), "1");
    }

    #[test]
    fn convex_read_back_applies_env_flags() {
        let fixture = make_fixture(None);
        fixture.create_dir_all("apps/api").unwrap();
        let config = fixture.config();
        let target_config = config.targets.convex.as_ref().unwrap();
        let runner = read_back_runner(b"A=1\n");
        let target = ConvexTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .read_back(&keys(&["A"]), &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["convex", "env", "list", "--prod"]);
    }

    #[test]
    fn convex_read_back_multiline_value_cannot_report_a_false_match() {
        // If a provider ever prints a value across multiple lines, the value
        // is truncated at the newline and continuation lines may appear as
        // phantom keys. That is noisy, but it fails toward drift, never toward
        // a false match — which is the property that matters. Pinned here so a
        // future "be lenient about continuation lines" change cannot quietly
        // turn a truncated read into a pass.
        let parsed = parse_env_list(b"PEM=-----BEGIN KEY-----\nabc123\n-----END KEY-----\n");
        assert_eq!(
            parsed["PEM"].as_str(),
            "-----BEGIN KEY-----",
            "a multi-line value is truncated at the newline"
        );

        let want: BTreeMap<String, Zeroizing<String>> = [(
            "PEM".to_string(),
            Zeroizing::new("-----BEGIN KEY-----\nabc123\n-----END KEY-----".to_string()),
        )]
        .into_iter()
        .collect();
        let findings =
            crate::verify::compare(Fidelity::Value, Ok(Evidence::Values(parsed)), &want);
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true },
            "a truncated read must surface as drift, never as a match"
        );
    }
}
