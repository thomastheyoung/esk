//! GitLab CI/CD target — deploys project variables via the `glab` CLI.
//!
//! GitLab CI/CD variables are injected into pipeline jobs. They can be scoped
//! to specific environments and optionally masked in job logs or protected
//! (only available on protected branches/tags).
//!
//! CLI: `glab` (GitLab's official CLI).
//! Commands: `glab variable set` / `glab variable delete`.
//!
//! Secrets are sent via **stdin** to avoid process argument exposure. Each
//! variable is scoped to an environment with `--scope <environment>`.

use anyhow::{Context, Result};

use std::collections::BTreeSet;

use crate::config::{Config, GitlabTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct GitlabTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a GitlabTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for GitlabTarget<'_> {
    fn name(&self) -> &'static str {
        "gitlab"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "glab")
            .context("Install from: https://gitlab.com/gitlab-org/cli")?;
        let output = self
            .runner
            .run("glab", &["auth", "status"], CommandOpts::default())
            .context("failed to run glab auth status")?;
        if !output.success {
            anyhow::bail!("glab is not authenticated. Run: glab auth login");
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variable", "set", key, "--scope", &target.environment];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "glab",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run glab variable set for {key}"))?
            .check("glab variable set", key)
    }

    /// Presence, despite GitLab returning values for most variables.
    ///
    /// `glab variable list -F json` does include `value`, and a `masked`
    /// variable still returns it — masking only affects job logs. But a
    /// variable created with `masked_and_hidden` (GitLab 17.4+) may not, and
    /// GitLab's API reference does not document which way that goes.
    ///
    /// [`Fidelity`] is declared per target, not per key, so esk cannot say
    /// "value for these, presence for that one". Claiming `Value` would make
    /// every hidden variable report as drift the operator cannot fix — the
    /// provider is withholding it by design. Presence is the claim that holds
    /// for every record, so it is the one esk makes.
    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Presence
    }

    /// List variable names via `glab variable list -F json`.
    ///
    /// Filters on `environment_scope`, since esk writes with `--scope <env>`
    /// and a variable scoped to another environment is not this scope's key.
    /// `--per-page` is set explicitly because glab defaults to 20 and would
    /// otherwise truncate silently — reporting every later key as missing.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        /// GitLab's maximum page size. Beyond this the listing needs paging,
        /// which is reported as an incomplete read rather than truncated.
        const PER_PAGE: &str = "100";

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variable", "list", "-F", "json", "--per-page", PER_PAGE];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("glab", &args, CommandOpts::default())
            .context("failed to run glab variable list")?;

        // A failed listing is an incomplete read, never an empty project.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("glab variable list failed: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse glab variable list JSON response")?;
        let entries = json
            .as_array()
            .context("glab variable list response was not a JSON array")?;

        // A full page may mean more variables exist that this listing does not
        // show. Rather than return a short set — which would report the
        // unlisted keys as missing — admit the read was incomplete.
        if entries.len() >= PER_PAGE.parse::<usize>().unwrap_or(usize::MAX) {
            anyhow::bail!(
                "glab returned a full page of {PER_PAGE} variables; the listing may be \
                 truncated and esk cannot confirm the rest"
            );
        }

        let present = entries
            .iter()
            .filter(|e| {
                // `*` is GitLab's all-environments scope and applies here too.
                match e.get("environment_scope").and_then(|s| s.as_str()) {
                    Some(scope) => scope == "*" || scope == target.environment,
                    // An entry whose scope esk cannot read is not evidence that
                    // the key is present here. Assuming it in-scope would hide
                    // a missing key, which is the drift-concealing direction.
                    None => false,
                }
            })
            .filter_map(|e| e.get("key")?.as_str().map(String::from))
            .collect();

        Ok(Evidence::Names {
            present,
            note: None,
        })
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["variable", "delete", key, "--scope", &target.environment];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("glab", &args, CommandOpts::default())
            .with_context(|| format!("failed to run glab variable delete for {key}"))?
            .check("glab variable delete", key)
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
  gitlab:
    env_flags:
      prod: "--masked"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "gitlab".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn gitlab_preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"Logged in".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].args, vec!["auth", "status"]);
    }

    #[test]
    fn gitlab_preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
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
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("glab is not authenticated"));
    }

    #[test]
    fn gitlab_preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("Install from: https://gitlab.com"));
    }

    #[test]
    fn gitlab_deploy_uses_stdin() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "glab");
        assert_eq!(
            calls[0].args,
            vec!["variable", "set", "MY_KEY", "--scope", "dev"]
        );
        // Value is passed via stdin, not in args
        assert_eq!(calls[0].stdin.as_deref(), Some(b"secret_val".as_slice()));
        assert!(!calls[0].args.iter().any(|a| a.contains("secret_val")));
    }

    #[test]
    fn gitlab_deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
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
            vec!["variable", "set", "KEY", "--scope", "prod", "--masked"]
        );
        assert_eq!(calls[0].stdin.as_deref(), Some(b"val".as_slice()));
    }

    #[test]
    fn gitlab_delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["variable", "delete", "MY_KEY", "--scope", "dev"]
        );
    }

    #[test]
    fn gitlab_delete_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("KEY", &make_target("prod")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["variable", "delete", "KEY", "--scope", "prod", "--masked"]
        );
    }

    #[test]
    fn gitlab_delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = GitlabTarget {
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
    fn gitlab_nonzero_exit() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"api error".to_vec(),
        }]);
        let target = GitlabTarget {
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
    ) -> std::collections::BTreeMap<String, zeroize::Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), zeroize::Zeroizing::new((*v).to_string())))
            .collect()
    }

    fn variable_list_json(entries: &[(&str, &str)]) -> Vec<u8> {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .map(|(key, scope)| {
                serde_json::json!({
                    "key": key,
                    "value": "some-value",
                    "environment_scope": scope,
                    "masked": true,
                    "hidden": false,
                })
            })
            .collect();
        serde_json::to_vec(&items).unwrap()
    }

    #[test]
    fn gitlab_read_back_lists_names_for_this_scope() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &variable_list_json(&[
                ("API_KEY", "dev"),
                ("EVERYWHERE", "*"),
                ("PROD_ONLY", "prod"),
            ]),
            b"",
        );
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target("dev"))
            .unwrap();
        let Evidence::Names { present, .. } = evidence else {
            panic!("gitlab declares Fidelity::Presence, so it must return Names");
        };
        assert!(present.contains("API_KEY"));
        assert!(
            present.contains("EVERYWHERE"),
            "`*` scope applies to every env"
        );
        assert!(
            !present.contains("PROD_ONLY"),
            "a variable scoped to another environment is not this scope's key"
        );
    }

    /// GitLab returns a `value` field, and a `Fidelity::Value` target would
    /// compare it. esk declares presence instead, because a `hidden` variable
    /// may withhold its value and would then report as drift the operator
    /// cannot fix. This pins that the value is never used as a verdict.
    #[test]
    fn gitlab_read_back_never_uses_the_returned_value() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        // The listing carries "some-value"; the store holds something else.
        runner.push_success(&variable_list_json(&[("API_KEY", "dev")]), b"");
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "a_completely_different_value")]),
        );
        let crate::verify::Findings::Presence { verdicts, .. } = &findings else {
            panic!("expected presence findings");
        };
        assert_eq!(
            verdicts["API_KEY"],
            crate::verify::PresenceVerdict::Present,
            "presence evidence cannot express a value comparison"
        );
    }

    #[test]
    fn gitlab_read_back_missing_key_is_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&variable_list_json(&[("OTHER", "dev")]), b"");
        let target = GitlabTarget {
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

    /// glab pages at 20 by default and silently drops the rest. A full page
    /// means the listing may be incomplete, and a short set would report every
    /// unlisted key as missing from GitLab.
    #[test]
    fn gitlab_read_back_full_page_is_an_incomplete_read() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let entries: Vec<(String, String)> = (0..100)
            .map(|i| (format!("KEY_{i:03}"), "dev".to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = entries
            .iter()
            .map(|(k, s)| (k.as_str(), s.as_str()))
            .collect();

        let runner = MockCommandRunner::new().strict();
        runner.push_success(&variable_list_json(&refs), b"");
        let target = GitlabTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["KEY_000"]), &make_target("dev")),
            &verify_expected(&[("KEY_000", "v")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Unresolved,
            "a possibly-truncated listing must not be reported as complete"
        );
    }

    #[test]
    fn gitlab_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"401 Unauthorized");
        let target = GitlabTarget {
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

    /// An entry with no readable scope must not be counted as present here:
    /// assuming it in-scope would conceal a genuinely missing key.
    #[test]
    fn gitlab_read_back_unknown_scope_is_not_assumed_present() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.gitlab.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(br#"[{"key":"API_KEY","value":"v"}]"#, b"");
        let target = GitlabTarget {
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
            crate::verify::Assessment::Resolved { drifted: true },
            "a scope-less entry must not be taken as this scope's key"
        );
    }
}
