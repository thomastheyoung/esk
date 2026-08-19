//! CircleCI target — deploys context secrets via the `circleci` CLI.
//!
//! CircleCI contexts are the primary mechanism for sharing secrets across
//! projects and pipelines. Secrets stored in a context are injected as
//! environment variables into jobs that reference that context.
//!
//! CLI: `circleci` (CircleCI's official CLI).
//! Commands: `circleci context store-secret` / `circleci context remove-secret`.
//!
//! Secrets are sent via **stdin** to avoid process argument exposure. Requires
//! `--org-id` and a context name to identify the target context.

use anyhow::{Context, Result};

use std::collections::BTreeSet;

use crate::config::{CircleciTargetConfig, Config, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct CircleciTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a CircleciTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for CircleciTarget<'_> {
    fn name(&self) -> &'static str {
        "circleci"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "circleci")
            .context("Install from: https://circleci.com/docs/local-cli/")?;
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let org_id = &self.target_config.org_id;
        let context = &self.target_config.context_name;
        let mut args: Vec<&str> = vec!["context", "store-secret", "--org-id", org_id, context, key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "circleci",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run circleci context store-secret for {key}"))?
            .check("circleci context store-secret", key)
    }

    /// Presence, permanently. CircleCI's docs are explicit that "variable
    /// values are never returned by the API once set".
    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Presence
    }

    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let org_id = &self.target_config.org_id;
        let context = &self.target_config.context_name;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        // `--org-id` matches the deploy's own flag: addressing the org
        // positionally would have the CLI read it as the context name, so the
        // read and the write would not refer to the same context.
        let mut args: Vec<&str> = vec![
            "context", "secret", "list", "--org-id", org_id, context, "--json",
        ];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("circleci", &args, CommandOpts::default())
            .context("failed to run circleci context secret list")?;

        // A failed listing is an incomplete read, never an empty context.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("circleci context secret list failed: {stderr}");
        }

        // `--json` rather than the rendered table. The table is styled on a
        // TTY, its header's first token is itself a valid key name, and its
        // empty case prints to stderr — each of which turns decoration into a
        // phantom key reported as drift, on every run, forever.
        //
        // The `truncated_value` field these records carry is deliberately
        // ignored: it is truncated, so comparing it could only ever produce
        // false verdicts. Presence is all CircleCI can prove.
        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse circleci context secret list JSON response")?;
        let entries = json
            .as_array()
            .context("circleci secret list response was not a JSON array")?;

        let present = entries
            .iter()
            .filter_map(|e| e.get("variable")?.as_str().map(String::from))
            .collect();

        Ok(Evidence::Names {
            present,
            note: None,
        })
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let org_id = &self.target_config.org_id;
        let context = &self.target_config.context_name;
        let mut args: Vec<&str> =
            vec!["context", "remove-secret", "--org-id", org_id, context, key];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("circleci", &args, CommandOpts::default())
            .with_context(|| format!("failed to run circleci context remove-secret for {key}"))?
            .check("circleci context remove-secret", key)
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
  circleci:
    org_id: "00000000-0000-0000-0000-000000000000"
    context_name: my-context
    env_flags:
      prod: "--some-flag value"
"#;
        ConfigFixture::new(yaml).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "circleci".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn circleci_preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"0.1.0".to_vec(),
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
    }

    #[test]
    fn circleci_preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Install from: https://circleci.com"));
    }

    #[test]
    fn circleci_deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "circleci");
        assert_eq!(
            calls[0].args,
            vec![
                "context",
                "store-secret",
                "--org-id",
                "00000000-0000-0000-0000-000000000000",
                "my-context",
                "MY_KEY"
            ]
        );
    }

    #[test]
    fn circleci_passes_value_via_stdin() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "my_secret", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].stdin.as_ref().unwrap(), b"my_secret");
    }

    #[test]
    fn circleci_deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
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
                "context",
                "store-secret",
                "--org-id",
                "00000000-0000-0000-0000-000000000000",
                "my-context",
                "KEY",
                "--some-flag",
                "value"
            ]
        );
    }

    #[test]
    fn circleci_delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "context",
                "remove-secret",
                "--org-id",
                "00000000-0000-0000-0000-000000000000",
                "my-context",
                "MY_KEY"
            ]
        );
        // No stdin for delete
        assert!(calls[0].stdin.is_none());
    }

    #[test]
    fn circleci_deploy_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }

    #[test]
    fn circleci_delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    /// A `circleci context secret list --json` response.
    fn secret_list_json(names: &[&str]) -> Vec<u8> {
        let items: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!({ "variable": n, "truncated_value": "xxxx" }))
            .collect();
        serde_json::to_vec(&items).unwrap()
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

    #[test]
    fn circleci_read_back_lists_names_only() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["API_KEY", "DB_URL"]), b"");
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target("dev"))
            .unwrap();
        let Evidence::Names { present, .. } = evidence else {
            panic!("circleci declares Fidelity::Presence, so it must return Names");
        };
        assert!(present.contains("API_KEY"));
        assert!(present.contains("DB_URL"));
        // The response's `truncated_value` field never enters the evidence:
        // presence evidence carries no values at all.
        assert!(!present.contains("xxxx"));

        let calls = runner.take_calls();
        assert!(
            calls[0].args.contains(&"--org-id".to_string()),
            "the org must be passed as a flag, matching the deploy; as a \
             positional the CLI would read it as the context name"
        );
        assert!(calls[0].args.contains(&"--json".to_string()));
    }

    #[test]
    fn circleci_read_back_missing_key_is_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["OTHER"]), b"");
        let target = CircleciTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
    }

    #[test]
    fn circleci_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.circleci.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"context not found");
        let target = CircleciTarget {
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
