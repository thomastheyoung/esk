//! Custom deploy target — runs user-defined commands from config.
//!
//! Custom targets let users define deploy/delete/preflight commands in `esk.yaml`
//! without writing Rust code. Template variables (`{{key}}`, `{{value}}`, `{{env}}`,
//! `{{app}}`) are substituted at deploy time.
//!
//! Only individual deploy mode is supported. Batch mode is out of scope.

use anyhow::{Context, Result};

use crate::config::{CustomTargetConfig, ResolvedTarget};
use crate::targets::{resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget};

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

        output.check(&format!("{} deploy", self.target_name), key)?;
        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let Some(ref cmd) = self.target_config.delete else {
            return Ok(());
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

        output.check(&format!("{} delete", self.target_name), key)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CustomCommandConfig;
    use crate::test_support::MockCommandRunner;
    use std::collections::BTreeMap;

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
            .deploy_secret("API_KEY", "secret123", &make_resolved("my-api", None, "prod"))
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
    fn delete_noop_when_unconfigured() {
        let config = make_target_config(vec!["set", "{{key}}"], None);
        let runner = MockCommandRunner::new();

        let target = CustomTarget {
            target_name: "my-api".to_string(),
            target_config: &config,
            runner: &runner,
        };
        // Should succeed without calling runner
        target
            .delete_secret("KEY", &make_resolved("my-api", None, "dev"))
            .unwrap();
        assert!(runner.take_calls().is_empty());
    }

    #[test]
    fn preflight_success() {
        let mut config = make_target_config(vec!["set", "{{key}}"], None);
        config.preflight = Some(CustomCommandConfig {
            program: "curl".to_string(),
            args: vec!["--fail".to_string(), "https://api.example.com/health".to_string()],
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
        assert_eq!(calls[0].args, vec!["--fail", "https://api.example.com/health"]);
    }

    #[test]
    fn preflight_failure() {
        let mut config = make_target_config(vec!["set", "{{key}}"], None);
        config.preflight = Some(CustomCommandConfig {
            program: "curl".to_string(),
            args: vec!["--fail".to_string(), "https://api.example.com/health".to_string()],
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
}
