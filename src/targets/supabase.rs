//! Supabase target — deploys Edge Function secrets via the `supabase` CLI.
//!
//! Supabase is an open-source Firebase alternative providing a Postgres
//! database, auth, realtime subscriptions, and Edge Functions (Deno-based
//! serverless). Secrets are available to Edge Functions as environment
//! variables.
//!
//! CLI: `supabase` (Supabase's official CLI).
//! Commands: `supabase secrets set` / `supabase secrets delete`.
//!
//! Secrets are set via **stdin** in `KEY=value` format. Requires a
//! `--project-ref` flag to identify the Supabase project. Values containing
//! newlines are rejected since the stdin `KEY=value` format cannot represent
//! them.

use anyhow::{Context, Result};

use crate::config::{Config, ResolvedTarget, SupabaseTargetConfig};
use crate::targets::{
    check_command, resolve_env_flags, validate_stdin_kv_value, CommandOpts, CommandRunner,
    DeployMode, DeployTarget,
};

pub struct SupabaseTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a SupabaseTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for SupabaseTarget<'_> {
    fn name(&self) -> &'static str {
        "supabase"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "supabase").map_err(|_| {
            anyhow::anyhow!(
                "supabase is not installed or not in PATH. Install it from: https://supabase.com/docs/guides/cli"
            )
        })?;
        let project_ref = &self.target_config.project_ref;
        let output = self
            .runner
            .run(
                "supabase",
                &["secrets", "list", "--project-ref", project_ref],
                CommandOpts::default(),
            )
            .context("failed to run supabase secrets list")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "supabase project '{project_ref}' not accessible. Run: supabase login\n{stderr}"
            );
        }
        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        validate_stdin_kv_value(key, value, "supabase")?;
        let project_ref = &self.target_config.project_ref;
        let stdin_data = format!("{key}={value}\n");

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "set", "--project-ref", project_ref];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run(
                "supabase",
                &args,
                CommandOpts {
                    stdin: Some(stdin_data.into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run supabase secrets set for {key}"))?
            .check("supabase secrets set", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let project_ref = &self.target_config.project_ref;

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "unset", key, "--project-ref", project_ref];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("supabase", &args, CommandOpts::default())
            .with_context(|| format!("failed to run supabase secrets unset for {key}"))?
            .check("supabase secrets unset", key)
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
  supabase:
    project_ref: abcdef123456
    env_flags:
      prod: "--experimental"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "supabase".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn supabase_preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"[]".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(
            calls[1].args,
            vec!["secrets", "list", "--project-ref", "abcdef123456"]
        );
    }

    #[test]
    fn supabase_preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"Unauthorized".to_vec(),
            },
        ]);
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("not accessible"));
    }

    #[test]
    fn supabase_preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("supabase is not installed"));
    }

    #[test]
    fn supabase_deploy_uses_stdin() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "supabase");
        assert_eq!(
            calls[0].args,
            vec!["secrets", "set", "--project-ref", "abcdef123456"]
        );
        // Value is passed via stdin, not in args
        assert_eq!(
            calls[0].stdin.as_deref(),
            Some(b"MY_KEY=secret_val\n".as_slice())
        );
        assert!(!calls[0].args.iter().any(|a| a.contains("secret_val")));
    }

    #[test]
    fn supabase_deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = SupabaseTarget {
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
                "secrets",
                "set",
                "--project-ref",
                "abcdef123456",
                "--experimental"
            ]
        );
        assert_eq!(calls[0].stdin.as_deref(), Some(b"KEY=val\n".as_slice()));
    }

    #[test]
    fn supabase_delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "secrets",
                "unset",
                "MY_KEY",
                "--project-ref",
                "abcdef123456"
            ]
        );
    }

    #[test]
    fn supabase_delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = SupabaseTarget {
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
    fn supabase_rejects_newline_in_value() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "line1\nline2", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn supabase_rejects_cr_in_value() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "line1\r\nline2", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn supabase_nonzero_exit() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"api error".to_vec(),
        }]);
        let target = SupabaseTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("api error"));
    }
}
