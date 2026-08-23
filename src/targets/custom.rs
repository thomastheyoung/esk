//! Custom deploy target — runs user-defined commands from config.
//!
//! Custom targets let users define deploy/delete/preflight commands in `esk.yaml`
//! without writing Rust code. Template variables (`{{key}}`, `{{value}}`, `{{env}}`,
//! `{{app}}`) are substituted at deploy time.
//!
//! Only individual deploy mode is supported. Batch mode is out of scope.

use anyhow::{Context, Result};

use std::collections::BTreeSet;

use crate::config::{CustomTargetConfig, ResolvedTarget};
use crate::targets::{resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget};
use crate::verify::{Evidence, Fidelity};

pub struct CustomTarget<'a> {
    pub target_name: String,
    pub target_config: &'a CustomTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

/// Substitute template variables in a string.
fn substitute(template: &str, key: &str, value: &str, target: &ResolvedTarget) -> String {
    template
        .replace("{{key}}", key)
        .replace("{{value}}", value)
        .replace("{{env}}", &target.environment)
        .replace("{{app}}", target.app.as_deref().unwrap_or(""))
}

/// Substitute template variables in args and append env_flags.
fn build_args(
    args: &[String],
    key: &str,
    value: &str,
    target: &ResolvedTarget,
    env_flags: &std::collections::BTreeMap<String, String>,
) -> Vec<String> {
    let mut result: Vec<String> = args
        .iter()
        .map(|a| substitute(a, key, value, target))
        .collect();
    result.extend(resolve_env_flags(env_flags, &target.environment));
    result
}

/// Check whether any args contain `{{value}}` (security concern: value in CLI args).
pub fn has_value_in_args(args: &[String]) -> bool {
    args.iter().any(|a| a.contains("{{value}}"))
}

impl DeployTarget for CustomTarget<'_> {
    fn name(&self) -> &str {
        &self.target_name
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    /// `Value` only when the user configured a `read:` command.
    ///
    /// esk cannot invent a way to read a service it knows nothing about, so
    /// the default stays `None` — an unconfigured custom target is reported
    /// as an honest gap rather than silently passing.
    fn verify_fidelity(&self) -> Fidelity {
        if self.target_config.read.is_some() {
            Fidelity::Value
        } else {
            Fidelity::None
        }
    }

    /// Run the user's `read:` command and parse its `KEY=VALUE` output.
    ///
    /// `{{key}}` and `{{value}}` are deliberately *not* substituted here. The
    /// command is expected to list the whole target in one invocation, and
    /// interpolating an expected value into a read command would hand the
    /// target the very thing withholding it is meant to prevent.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let Some(read) = &self.target_config.read else {
            return Ok(Evidence::Unreadable(
                "no `read:` command configured for this custom target",
            ));
        };

        let args = build_args(&read.args, "", "", target, &self.target_config.env_flags);
        let args_str: Vec<&str> = args.iter().map(String::as_str).collect();

        let output = self
            .runner
            .run(&read.program, &args_str, CommandOpts::default())
            .with_context(|| {
                format!(
                    "custom target '{}': read command '{}' failed to execute",
                    self.target_name, read.program
                )
            })?;

        // A failed read is an incomplete read, never an empty target: an empty
        // map would report every managed key as missing.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "custom target '{}': read failed: {}",
                self.target_name,
                stderr.trim()
            );
        }

        // Values are taken verbatim: a `read:` command must print the stored
        // value unquoted, since esk cannot know which quoting convention an
        // arbitrary script uses. It must also print *only* `KEY=VALUE` lines —
        // the shared parser refuses anything else rather than guessing.
        //
        // A previous version skipped lines whose key failed `validate_key`, to
        // tolerate banner text like `Fetching secrets for env=dev`. That filter
        // could not tell a banner from the continuation of a multiline secret,
        // so it silently truncated such values and emitted their remaining
        // lines as phantom keys — which `verify` prints in the `extra` list,
        // leaking fragments of the plaintext. Refusing is the honest answer;
        // a `read:` command that prints banners must send them to stderr.
        let values = crate::targets::parse_kv_read_back(
            &output.stdout,
            &format!("custom target '{}'", self.target_name),
        )?;

        Ok(Evidence::Values(values))
    }

    fn preflight(&self) -> Result<()> {
        let Some(ref pf) = self.target_config.preflight else {
            return Ok(());
        };
        let args_str: Vec<&str> = pf.args.iter().map(String::as_str).collect();
        let output = self
            .runner
            .run(&pf.program, &args_str, CommandOpts::default())
            .with_context(|| {
                format!(
                    "custom target '{}': preflight command '{}' failed to execute",
                    self.target_name, pf.program
                )
            })?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "custom target '{}': preflight failed: {}",
                self.target_name,
                stderr.trim()
            );
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let cmd = &self.target_config.deploy;
        let args = build_args(&cmd.args, key, value, target, &self.target_config.env_flags);
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();

        let stdin = cmd
            .stdin
            .as_ref()
            .map(|s| substitute(s, key, value, target).into_bytes());

        let output = self
            .runner
            .run(
                &cmd.program,
                &args_ref,
                CommandOpts {
                    stdin,
                    ..Default::default()
                },
            )
            .with_context(|| {
                format!(
                    "custom target '{}': deploy command failed for {key}",
                    self.target_name
                )
            })?;

        output.check(&format!("{} deploy", self.target_name), key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let Some(ref cmd) = self.target_config.delete else {
            anyhow::bail!(
                "custom target '{}': deletion is unsupported because no delete command is configured",
                self.target_name
            );
        };
        let args = build_args(&cmd.args, key, "", target, &self.target_config.env_flags);
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();

        let stdin = cmd
            .stdin
            .as_ref()
            .map(|s| substitute(s, key, "", target).into_bytes());

        let output = self
            .runner
            .run(
                &cmd.program,
                &args_ref,
                CommandOpts {
                    stdin,
                    ..Default::default()
                },
            )
            .with_context(|| {
                format!(
                    "custom target '{}': delete command failed for {key}",
                    self.target_name
                )
            })?;

        output.check(&format!("{} delete", self.target_name), key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CustomCommandConfig;
    use crate::test_support::MockCommandRunner;
    use std::collections::BTreeMap;
    use zeroize::Zeroizing;

    fn make_target_config(
        deploy_args: Vec<&str>,
        deploy_stdin: Option<&str>,
    ) -> CustomTargetConfig {
        CustomTargetConfig {
            deploy: CustomCommandConfig {
                program: "my-tool".to_string(),
                args: deploy_args.into_iter().map(String::from).collect(),
                stdin: deploy_stdin.map(String::from),
            },
            delete: None,
            preflight: None,
            read: None,
            env_flags: BTreeMap::new(),
        }
    }

    fn make_resolved(service: &str, app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: service.to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    #[test]
    fn substitute_all_vars() {
        let result = substitute(
            "{{key}}={{value}} env={{env}} app={{app}}",
            "MY_KEY",
            "secret",
            &make_resolved("test", Some("web"), "prod"),
        );
        assert_eq!(result, "MY_KEY=secret env=prod app=web");
    }

    #[test]
    fn substitute_empty_app() {
        let result = substitute(
            "{{app}}/{{key}}",
            "KEY",
            "val",
            &make_resolved("test", None, "dev"),
        );
        assert_eq!(result, "/KEY");
    }

    #[test]
    fn build_args_with_env_flags() {
        let args = vec!["set".to_string(), "{{key}}".to_string()];
        let mut env_flags = BTreeMap::new();
        env_flags.insert("prod".to_string(), "--force --verbose".to_string());

        let result = build_args(
            &args,
            "API_KEY",
            "val",
            &make_resolved("test", None, "prod"),
            &env_flags,
        );
        assert_eq!(result, vec!["set", "API_KEY", "--force", "--verbose"]);
    }

    #[test]
    fn build_args_no_env_flags() {
        let args = vec!["deploy".to_string(), "{{key}}".to_string()];
        let result = build_args(
            &args,
            "KEY",
            "val",
            &make_resolved("test", None, "dev"),
            &BTreeMap::new(),
        );
        assert_eq!(result, vec!["deploy", "KEY"]);
    }

    #[test]
    fn has_value_in_args_detects() {
        assert!(has_value_in_args(&[
            "-d".to_string(),
            "{{value}}".to_string()
        ]));
    }

    #[test]
    fn has_value_in_args_absent() {
        assert!(!has_value_in_args(&[
            "-d".to_string(),
            "{{key}}".to_string()
        ]));
    }

    #[test]
    fn deploy_calls_runner_with_substituted_args() {
        let config = make_target_config(vec!["set", "{{key}}", "--env", "{{env}}"], None);
        let runner = MockCommandRunner::new();
        runner.push_success(b"", b"");

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        target
            .deploy_secret(
                "API_KEY",
                "secret123",
                &make_resolved("my-api", None, "prod"),
            )
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "my-tool");
        assert_eq!(calls[0].args, vec!["set", "API_KEY", "--env", "prod"]);
        assert!(calls[0].stdin.is_none());
    }

    #[test]
    fn deploy_passes_stdin_template() {
        let config = make_target_config(vec!["set", "{{key}}"], Some("{{value}}"));
        let runner = MockCommandRunner::new();
        runner.push_success(b"", b"");

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "my_secret", &make_resolved("my-api", None, "dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls[0].stdin.as_deref(), Some(b"my_secret".as_slice()));
    }

    #[test]
    fn deploy_nonzero_exit_propagates_error() {
        let config = make_target_config(vec!["set", "{{key}}"], None);
        let runner = MockCommandRunner::new();
        runner.push_failure(b"access denied");

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_resolved("my-api", None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn delete_calls_runner() {
        let mut config = make_target_config(vec!["set", "{{key}}"], None);
        config.delete = Some(CustomCommandConfig {
            program: "my-tool".to_string(),
            args: vec!["rm".to_string(), "{{key}}".to_string()],
            stdin: None,
        });
        let runner = MockCommandRunner::new();
        runner.push_success(b"", b"");

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        target
            .delete_secret("OLD_KEY", &make_resolved("my-api", None, "dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].program, "my-tool");
        assert_eq!(calls[0].args, vec!["rm", "OLD_KEY"]);
    }

    #[test]
    fn delete_without_command_is_not_acknowledged() {
        let config = make_target_config(vec!["set", "{{key}}"], None);
        let runner = MockCommandRunner::new();

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        let error = target
            .delete_secret("KEY", &make_resolved("my-api", None, "dev"))
            .unwrap_err();
        assert!(error.to_string().contains("deletion is unsupported"));
        assert!(!error.to_string().contains("KEY"));
        assert!(runner.take_calls().is_empty());
    }

    #[test]
    fn preflight_success() {
        let mut config = make_target_config(vec!["set", "{{key}}"], None);
        config.preflight = Some(CustomCommandConfig {
            program: "curl".to_string(),
            args: vec![
                "--fail".to_string(),
                "https://api.example.com/health".to_string(),
            ],
            stdin: None,
        });
        let runner = MockCommandRunner::new();
        runner.push_success(b"OK", b"");

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "curl");
        assert_eq!(
            calls[0].args,
            vec!["--fail", "https://api.example.com/health"]
        );
    }

    #[test]
    fn preflight_failure() {
        let mut config = make_target_config(vec!["set", "{{key}}"], None);
        config.preflight = Some(CustomCommandConfig {
            program: "curl".to_string(),
            args: vec![
                "--fail".to_string(),
                "https://api.example.com/health".to_string(),
            ],
            stdin: None,
        });
        let runner = MockCommandRunner::new();
        runner.push_failure(b"connection refused");

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("preflight failed"));
        assert!(err.to_string().contains("connection refused"));
    }

    #[test]
    fn preflight_noop_when_unconfigured() {
        let config = make_target_config(vec!["set", "{{key}}"], None);
        let runner = MockCommandRunner::new();

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        assert!(runner.take_calls().is_empty());
    }

    fn verify_keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn verify_expected(pairs: &[(&str, &str)]) -> BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    fn with_read(mut cfg: CustomTargetConfig, args: Vec<&str>) -> CustomTargetConfig {
        cfg.read = Some(CustomCommandConfig {
            program: "my-tool".to_string(),
            args: args.into_iter().map(String::from).collect(),
            stdin: None,
        });
        cfg
    }

    /// Without a `read:` command esk has no way to look, and says so rather
    /// than passing silently.
    #[test]
    fn custom_without_a_read_command_is_unverifiable() {
        let cfg = make_target_config(vec!["set", "{{key}}"], None);
        let runner = MockCommandRunner::new().strict();
        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &cfg,
            runner: &runner,
        };

        assert_eq!(target.verify_fidelity(), Fidelity::None);
        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["A"]), &make_resolved("my-api", None, "dev")),
            &verify_expected(&[("A", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
        assert!(matches!(
            findings,
            crate::verify::Findings::Unverifiable { .. }
        ));
        // No command was run: there was nothing to run.
        assert!(runner.take_calls().is_empty());
    }

    #[test]
    fn custom_with_a_read_command_returns_values() {
        let cfg = with_read(
            make_target_config(vec!["set", "{{key}}"], None),
            vec!["list", "--env", "{{env}}"],
        );
        let runner = MockCommandRunner::new().strict();
        runner.push_success(b"API_KEY=secret1\nDB_URL=postgres://x\n", b"");
        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &cfg,
            runner: &runner,
        };

        assert_eq!(target.verify_fidelity(), Fidelity::Value);
        let evidence = target
            .read_back(
                &verify_keys(&["API_KEY"]),
                &make_resolved("my-api", None, "dev"),
            )
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("a configured read command declares Value fidelity");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "my-tool");
        assert_eq!(calls[0].args, vec!["list", "--env", "dev"]);
    }

    #[test]
    fn custom_read_back_surfaces_wrong_value_as_drift() {
        let cfg = with_read(
            make_target_config(vec!["set", "{{key}}"], None),
            vec!["list"],
        );
        let runner = MockCommandRunner::new().strict();
        runner.push_success(b"API_KEY=STALE\n", b"");
        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &cfg,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(
                &verify_keys(&["API_KEY"]),
                &make_resolved("my-api", None, "dev"),
            ),
            &verify_expected(&[("API_KEY", "current")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
    }

    #[test]
    fn custom_read_back_failure_is_unreachable_not_empty() {
        let cfg = with_read(
            make_target_config(vec!["set", "{{key}}"], None),
            vec!["list"],
        );
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"connection refused");
        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &cfg,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(
                &verify_keys(&["API_KEY"]),
                &make_resolved("my-api", None, "dev"),
            ),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
    }

    /// A read command's output is a user script's stdout, which commonly
    /// carries progress lines. Anything that is not a valid key name is not a
    /// key, or it would be reported as unmanaged drift on every run.
    #[test]
    fn custom_read_back_ignores_banner_lines() {
        let cfg = with_read(
            make_target_config(vec!["set", "{{key}}"], None),
            vec!["list"],
        );
        let runner = MockCommandRunner::new().strict();
        runner.push_success(b"Fetching secrets for env=dev\nAPI_KEY=real\n", b"");
        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &cfg,
            runner: &runner,
        };

        let evidence = target
            .read_back(
                &verify_keys(&["API_KEY"]),
                &make_resolved("my-api", None, "dev"),
            )
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values");
        };
        assert_eq!(values.len(), 1, "only the real key is a key");
        assert_eq!(values["API_KEY"].as_str(), "real");
    }

    /// A line with no `=` makes the read unusable rather than merely noisy.
    ///
    /// esk cannot tell a separator-less banner from the continuation of a
    /// multiline secret. Skipping it would truncate that secret — reporting
    /// drift the operator can never clear — and emit its remaining lines as
    /// phantom keys carrying fragments of the plaintext, which `verify` prints
    /// verbatim because `redact_exact` only matches a whole value.
    #[test]
    fn custom_read_back_refuses_output_it_cannot_represent() {
        let cfg = with_read(
            make_target_config(vec!["set", "{{key}}"], None),
            vec!["list"],
        );
        let runner = MockCommandRunner::new().strict();
        runner.push_success(b"PEM=-----BEGIN KEY-----\nabc123\n-----END KEY-----\n", b"");
        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &cfg,
            runner: &runner,
        };

        // `Evidence` deliberately has no `Debug` — it carries secret values —
        // so the error is destructured rather than unwrapped.
        let Err(err) = target.read_back(
            &verify_keys(&["PEM"]),
            &make_resolved("my-api", None, "dev"),
        ) else {
            panic!("output the grammar cannot represent must not parse");
        };
        assert!(
            err.to_string().contains("no '=' separator"),
            "error was: {err}"
        );

        // And the refusal must reach the report as "not established", never as
        // a verdict about the value.
        let findings = crate::verify::compare(
            Fidelity::Value,
            Err(err),
            &verify_expected(&[("PEM", "-----BEGIN KEY-----\nabc123\n-----END KEY-----")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Unresolved,
            "a value the grammar cannot carry must not produce a verdict"
        );
    }
}
