//! GitHub Actions target — deploys repository secrets via the `gh` CLI.
//!
//! GitHub Actions secrets are encrypted environment variables available to
//! workflow runs. They are stored using libsodium sealed-box encryption on
//! GitHub's servers.
//!
//! CLI: `gh` (GitHub's official CLI).
//! Commands: `gh secret set` / `gh secret delete`.
//!
//! Secrets are sent via **stdin** to avoid process argument exposure. Supports
//! an optional `-R <owner/repo>` flag to target a specific repository (defaults
//! to the current repo).

use anyhow::{Context, Result};

use std::collections::BTreeSet;

use crate::config::{Config, GithubTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct GithubTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a GithubTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for GithubTarget<'_> {
    fn name(&self) -> &'static str {
        "github"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "gh").context("Install from: https://cli.github.com/")?;
        let output = self
            .runner
            .run("gh", &["auth", "status"], CommandOpts::default())
            .context("failed to run gh auth status")?;
        if !output.success {
            anyhow::bail!("gh is not authenticated. Run: gh auth login");
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "set", key];
        if let Some(repo) = &self.target_config.repo {
            args.push("-R");
            args.push(repo);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "gh",
                &args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run gh secret set for {key}"))?
            .check("gh secret set", key)
    }

    /// Presence, permanently. `gh secret list` exposes name, visibility, and
    /// `updatedAt` — never a value. GitHub secrets are write-only by design.
    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Presence
    }

    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "list", "--json", "name"];
        if let Some(repo) = &self.target_config.repo {
            args.push("-R");
            args.push(repo);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("gh", &args, CommandOpts::default())
            .context("failed to run gh secret list")?;

        // A failed listing is an incomplete read, never an empty repository.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("gh secret list failed: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse gh secret list JSON response")?;
        let entries = json
            .as_array()
            .context("gh secret list response was not a JSON array")?;

        Ok(Evidence::Names {
            present: entries
                .iter()
                .filter_map(|e| e.get("name")?.as_str().map(String::from))
                .collect(),
            note: None,
        })
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secret", "delete", key];
        if let Some(repo) = &self.target_config.repo {
            args.push("-R");
            args.push(repo);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("gh", &args, CommandOpts::default())
            .with_context(|| format!("failed to run gh secret delete for {key}"))?
            .check("gh secret delete", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_config(with_repo: bool) -> ConfigFixture {
        let yaml = if with_repo {
            r#"
project: x
environments: [dev, prod]
targets:
  github:
    repo: owner/repo
    env_flags:
      prod: "--env production"
"#
        } else {
            r#"
project: x
environments: [dev, prod]
targets:
  github:
    env_flags:
      prod: "--env production"
"#
        };
        ConfigFixture::new(yaml).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "github".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn github_preflight_success() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"Logged in".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].args, vec!["auth", "status"]);
    }

    #[test]
    fn github_preflight_auth_failure() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not logged in".to_vec(),
            },
        ]);
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("gh is not authenticated"));
    }

    #[test]
    fn github_preflight_missing_cli() {
        let fixture = make_config(false);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Install from: https://cli.github.com/"));
    }

    #[test]
    fn github_deploy_correct_args_with_repo() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "gh");
        assert_eq!(
            calls[0].args,
            vec!["secret", "set", "MY_KEY", "-R", "owner/repo"]
        );
    }

    #[test]
    fn github_passes_value_via_stdin() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GithubTarget {
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
    fn github_deploy_without_repo() {
        let fixture = make_config(false);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["secret", "set", "KEY"]);
    }

    #[test]
    fn github_deploy_with_env_flags() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GithubTarget {
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
                "secret",
                "set",
                "KEY",
                "-R",
                "owner/repo",
                "--env",
                "production"
            ]
        );
    }

    #[test]
    fn github_delete_correct_args() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["secret", "delete", "MY_KEY", "-R", "owner/repo"]
        );
    }

    #[test]
    fn github_delete_failure() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = GithubTarget {
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
    fn github_nonzero_exit() {
        let fixture = make_config(false);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
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
            .map(|n| serde_json::json!({ "name": n }))
            .collect();
        serde_json::to_vec(&items).unwrap()
    }

    #[test]
    fn github_read_back_lists_secret_names() {
        let fixture = make_config(true);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["API_KEY"]), b"");
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target("dev"))
            .unwrap();
        let Evidence::Names { present, .. } = evidence else {
            panic!("github declares Fidelity::Presence, so it must return Names");
        };
        assert!(present.contains("API_KEY"));

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "gh");
        assert_eq!(
            calls[0].args,
            vec!["secret", "list", "--json", "name", "-R", "owner/repo"]
        );
    }

    /// GitHub secrets are write-only. Even with a value in the store, the
    /// strongest available claim is that the key exists.
    #[test]
    fn github_read_back_cannot_claim_a_value_matched() {
        let fixture = make_config(false);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["API_KEY"]), b"");
        let target = GithubTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        let crate::verify::Findings::Presence { verdicts, .. } = &findings else {
            panic!("expected presence findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::PresenceVerdict::Present);
    }

    #[test]
    fn github_read_back_missing_key_is_drift() {
        let fixture = make_config(false);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&secret_list_json(&["OTHER"]), b"");
        let target = GithubTarget {
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
    fn github_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config(false);
        let config = fixture.config();
        let target_config = config.targets.github.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"HTTP 401: Bad credentials");
        let target = GithubTarget {
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
