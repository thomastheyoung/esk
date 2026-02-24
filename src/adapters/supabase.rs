use anyhow::{Context, Result};

use crate::adapters::{
    append_env_flags, check_command, resolve_env_flags, validate_stdin_kv_value, CommandOpts,
    CommandRunner, SyncAdapter, SyncMode,
};
use crate::config::{Config, ResolvedTarget, SupabaseAdapterConfig};

pub struct SupabaseAdapter<'a> {
    pub config: &'a Config,
    pub adapter_config: &'a SupabaseAdapterConfig,
    pub runner: &'a dyn CommandRunner,
}

impl<'a> SyncAdapter for SupabaseAdapter<'a> {
    fn name(&self) -> &str {
        "supabase"
    }

    fn sync_mode(&self) -> SyncMode {
        SyncMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "supabase").map_err(|_| {
            anyhow::anyhow!(
                "supabase is not installed or not in PATH. Install it from: https://supabase.com/docs/guides/cli"
            )
        })?;

        let project_ref = &self.adapter_config.project_ref;
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
                "supabase project '{project_ref}' not accessible (not logged in or invalid project ref): {stderr}"
            );
        }

        Ok(())
    }

    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        validate_stdin_kv_value(key, value, "supabase")?;
        let project_ref = &self.adapter_config.project_ref;
        let stdin_data = format!("{key}={value}\n");

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "set", "--project-ref", project_ref];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run(
                "supabase",
                &args,
                CommandOpts {
                    stdin: Some(stdin_data.into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run supabase for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("supabase secrets set failed for {key}: {stderr}");
        }

        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let project_ref = &self.adapter_config.project_ref;

        let flag_parts = resolve_env_flags(&self.adapter_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["secrets", "unset", key, "--project-ref", project_ref];
        append_env_flags(&mut args, &flag_parts);

        let output = self
            .runner
            .run("supabase", &args, CommandOpts::default())
            .with_context(|| format!("failed to run supabase delete for {key}"))?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("supabase secrets unset failed for {key}: {stderr}");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (String, Vec<String>, Option<Vec<u8>>);

    fn take_calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .take_calls()
            .into_iter()
            .map(|call| (call.program, call.args, call.stdin))
            .collect()
    }

    fn make_config(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: x
environments: [dev, prod]
adapters:
  supabase:
    project_ref: abcdef123456
    env_flags:
      prod: "--experimental"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            adapter: "supabase".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn supabase_preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
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
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        assert!(adapter.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(
            calls[1].1,
            vec!["secrets", "list", "--project-ref", "abcdef123456"]
        );
    }

    #[test]
    fn supabase_preflight_auth_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
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
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("not accessible"));
    }

    #[test]
    fn supabase_preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter.preflight().unwrap_err();
        assert!(err.to_string().contains("supabase is not installed"));
    }

    #[test]
    fn supabase_sync_uses_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "supabase");
        assert_eq!(
            calls[0].1,
            vec!["secrets", "set", "--project-ref", "abcdef123456"]
        );
        // Value is passed via stdin, not in args
        assert_eq!(
            calls[0].2.as_deref(),
            Some(b"MY_KEY=secret_val\n".as_slice())
        );
        assert!(!calls[0].1.iter().any(|a| a.contains("secret_val")));
    }

    #[test]
    fn supabase_sync_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .sync_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
            vec![
                "secrets",
                "set",
                "--project-ref",
                "abcdef123456",
                "--experimental"
            ]
        );
        assert_eq!(calls[0].2.as_deref(), Some(b"KEY=val\n".as_slice()));
    }

    #[test]
    fn supabase_delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        adapter
            .delete_secret("MY_KEY", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(
            calls[0].1,
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
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn supabase_rejects_newline_in_value() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "line1\nline2", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn supabase_rejects_cr_in_value() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "line1\r\nline2", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn supabase_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let adapter_config = config.adapters.supabase.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"api error".to_vec(),
        }]);
        let adapter = SupabaseAdapter {
            config: &config,
            adapter_config,
            runner: &runner,
        };
        let err = adapter
            .sync_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("api error"));
    }
}
