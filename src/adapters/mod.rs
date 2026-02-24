pub mod aws_ssm;
pub mod cloudflare;
pub mod convex;
pub mod env_file;
pub mod fly;
pub mod github;
pub mod gitlab;
pub mod heroku;
pub mod kubernetes;
pub mod netlify;
pub mod railway;
pub mod supabase;
pub mod vercel;

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::{Config, ResolvedTarget};

pub struct SyncResult {
    pub key: String,
    #[allow(dead_code)]
    pub target: ResolvedTarget,
    pub success: bool,
    pub error: Option<String>,
}

/// Secret with its key and value, ready for syncing.
#[derive(Clone)]
pub struct SecretValue {
    pub key: String,
    pub value: String,
    pub vendor: String,
}

/// Whether an adapter syncs secrets individually or as a batch per target group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Sync each secret individually (e.g. cloudflare, convex).
    Individual,
    /// Regenerate the entire target in one batch (e.g. env files).
    Batch,
}

/// Options for running an external command.
#[derive(Default)]
pub struct CommandOpts {
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
}

/// Output from an external command.
pub struct CommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Abstraction over `std::process::Command` for testability.
pub trait CommandRunner: Send + Sync {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput>;
}

/// Production implementation that shells out to real processes.
pub struct RealCommandRunner;

impl CommandRunner for RealCommandRunner {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let mut cmd = Command::new(program);
        cmd.args(args);

        if let Some(cwd) = &opts.cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }

        if opts.stdin.is_some() {
            cmd.stdin(Stdio::piped());
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn()?;

        if let Some(input) = &opts.stdin {
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(input)?;
            }
        }

        let output = child.wait_with_output()?;
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

pub trait SyncAdapter {
    fn name(&self) -> &str;

    /// Whether this adapter syncs individually or in batches.
    fn sync_mode(&self) -> SyncMode;

    /// Validate that external dependencies are available before syncing.
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Sync a single secret to a target.
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;

    /// Delete a single secret from a target. Default: no-op (batch adapters handle deletion
    /// by regenerating the full output without the deleted key).
    fn delete_secret(&self, _key: &str, _target: &ResolvedTarget) -> Result<()> {
        Ok(())
    }

    /// Sync a batch of secrets. Default implementation loops sync_secret.
    fn sync_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<SyncResult> {
        secrets
            .iter()
            .map(|s| match self.sync_secret(&s.key, &s.value, target) {
                Ok(()) => SyncResult {
                    key: s.key.clone(),
                    target: target.clone(),
                    success: true,
                    error: None,
                },
                Err(e) => SyncResult {
                    key: s.key.clone(),
                    target: target.clone(),
                    success: false,
                    error: Some(e.to_string()),
                },
            })
            .collect()
    }
}

/// Validate that a secret value is safe for stdin-based KEY=VALUE adapters.
///
/// Adapters like Fly and Supabase pass `KEY=VALUE\n` via stdin. A newline
/// in the value would inject additional environment variables.
pub fn validate_stdin_kv_value(key: &str, value: &str, adapter_name: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!(
            "{adapter_name}: secret '{key}' contains newlines, which would inject \
             additional variables via stdin. Remove newlines or use a different adapter."
        );
    }
    Ok(())
}

/// Resolve env_flags for a given environment into split parts.
/// Returns an empty vec if no flags are configured for the environment.
pub fn resolve_env_flags(flags: &BTreeMap<String, String>, env: &str) -> Vec<String> {
    flags
        .get(env)
        .filter(|s| !s.is_empty())
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Append resolved env flag parts to an args vector.
pub fn append_env_flags<'a>(args: &mut Vec<&'a str>, flag_parts: &'a [String]) {
    for part in flag_parts {
        args.push(part);
    }
}

/// Check that an external command is available via the CommandRunner.
pub fn check_command(runner: &dyn CommandRunner, program: &str) -> Result<()> {
    runner
        .run(program, &["--version"], CommandOpts::default())
        .map_err(|_| {
            anyhow::anyhow!("{program} is not installed or not in PATH. Install it and try again.")
        })?;
    Ok(())
}

/// Health status of a configured adapter.
pub struct AdapterHealth {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

struct AdapterCandidate<'a> {
    adapter: Box<dyn SyncAdapter + 'a>,
    ok_message: &'static str,
}

fn adapter_candidates<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<AdapterCandidate<'a>> {
    let mut candidates: Vec<AdapterCandidate<'a>> = Vec::new();

    if config.adapters.env.is_some() {
        candidates.push(AdapterCandidate {
            adapter: Box::new(env_file::EnvFileAdapter { config }),
            ok_message: "writable",
        });
    }

    if let Some(adapter_config) = &config.adapters.cloudflare {
        candidates.push(AdapterCandidate {
            adapter: Box::new(cloudflare::CloudflareAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "wrangler authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.convex {
        candidates.push(AdapterCandidate {
            adapter: Box::new(convex::ConvexAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "deployment accessible",
        });
    }

    if let Some(adapter_config) = &config.adapters.fly {
        candidates.push(AdapterCandidate {
            adapter: Box::new(fly::FlyAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "fly authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.netlify {
        candidates.push(AdapterCandidate {
            adapter: Box::new(netlify::NetlifyAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "netlify linked",
        });
    }

    if let Some(adapter_config) = &config.adapters.vercel {
        candidates.push(AdapterCandidate {
            adapter: Box::new(vercel::VercelAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "vercel authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.github {
        candidates.push(AdapterCandidate {
            adapter: Box::new(github::GithubAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "gh authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.heroku {
        candidates.push(AdapterCandidate {
            adapter: Box::new(heroku::HerokuAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "heroku authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.supabase {
        candidates.push(AdapterCandidate {
            adapter: Box::new(supabase::SupabaseAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "supabase available",
        });
    }

    if let Some(adapter_config) = &config.adapters.railway {
        candidates.push(AdapterCandidate {
            adapter: Box::new(railway::RailwayAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "railway authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.gitlab {
        candidates.push(AdapterCandidate {
            adapter: Box::new(gitlab::GitlabAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "glab authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.aws_ssm {
        candidates.push(AdapterCandidate {
            adapter: Box::new(aws_ssm::AwsSsmAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "aws authenticated",
        });
    }

    if let Some(adapter_config) = &config.adapters.kubernetes {
        candidates.push(AdapterCandidate {
            adapter: Box::new(kubernetes::KubernetesAdapter {
                config,
                adapter_config,
                runner,
            }),
            ok_message: "kubectl available",
        });
    }

    candidates
}

fn needs_cli_secret_arg_warning(name: &str) -> bool {
    matches!(name, "convex" | "netlify" | "heroku" | "railway")
}

/// Check the health of all configured adapters without filtering.
/// Returns one entry per configured adapter with preflight pass/fail.
pub fn check_adapter_health(config: &Config, runner: &dyn CommandRunner) -> Vec<AdapterHealth> {
    let mut health = Vec::new();
    for candidate in adapter_candidates(config, runner) {
        let name = candidate.adapter.name().to_string();
        match candidate.adapter.preflight() {
            Ok(()) => health.push(AdapterHealth {
                name,
                ok: true,
                message: candidate.ok_message.to_string(),
            }),
            Err(e) => health.push(AdapterHealth {
                name,
                ok: false,
                message: e.to_string(),
            }),
        }
    }
    health
}

/// Build all configured sync adapters from the config.
/// Runs preflight checks and filters out adapters that fail, printing warnings.
pub fn build_sync_adapters<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<Box<dyn SyncAdapter + 'a>> {
    let mut adapters: Vec<Box<dyn SyncAdapter + 'a>> = Vec::new();

    for candidate in adapter_candidates(config, runner) {
        let adapter = candidate.adapter;
        match adapter.preflight() {
            Ok(()) => {
                if needs_cli_secret_arg_warning(adapter.name()) {
                    let _ = cliclack::log::warning(format!(
                        "{}: secrets passed via CLI arguments (visible in process listings)",
                        adapter.name()
                    ));
                }
                adapters.push(adapter);
            }
            Err(e) => {
                let _ =
                    cliclack::log::warning(format!("Skipping {} adapter: {}", adapter.name(), e));
            }
        }
    }

    adapters
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestAdapter {
        fail_keys: Vec<String>,
    }

    impl SyncAdapter for TestAdapter {
        fn name(&self) -> &str {
            "test"
        }

        fn sync_mode(&self) -> SyncMode {
            SyncMode::Individual
        }

        fn sync_secret(&self, key: &str, _value: &str, _target: &ResolvedTarget) -> Result<()> {
            if self.fail_keys.contains(&key.to_string()) {
                anyhow::bail!("sync failed for {key}");
            }
            Ok(())
        }
    }

    fn make_target() -> ResolvedTarget {
        ResolvedTarget {
            adapter: "test".to_string(),
            app: Some("web".to_string()),
            environment: "dev".to_string(),
        }
    }

    fn make_secret(key: &str) -> SecretValue {
        SecretValue {
            key: key.to_string(),
            value: "val".to_string(),
            vendor: "G".to_string(),
        }
    }

    #[test]
    fn default_sync_batch_all_success() {
        let adapter = TestAdapter { fail_keys: vec![] };
        let secrets = vec![make_secret("A"), make_secret("B")];
        let results = adapter.sync_batch(&secrets, &make_target());
        assert!(results.iter().all(|r| r.success));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn default_sync_batch_partial_failure() {
        let adapter = TestAdapter {
            fail_keys: vec!["B".to_string()],
        };
        let secrets = vec![make_secret("A"), make_secret("B"), make_secret("C")];
        let results = adapter.sync_batch(&secrets, &make_target());
        assert!(results[0].success);
        assert!(!results[1].success);
        assert!(results[1].error.is_some());
        assert!(results[2].success);
    }

    #[test]
    fn default_sync_batch_empty_input() {
        let adapter = TestAdapter { fail_keys: vec![] };
        let results = adapter.sync_batch(&[], &make_target());
        assert!(results.is_empty());
    }

    #[test]
    fn check_command_success() {
        struct OkRunner;
        impl CommandRunner for OkRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: b"1.0.0".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }
        assert!(check_command(&OkRunner, "some-tool").is_ok());
    }

    #[test]
    fn check_command_missing() {
        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("No such file or directory")
            }
        }
        let err = check_command(&FailRunner, "missing-tool").unwrap_err();
        assert!(err.to_string().contains("missing-tool is not installed"));
    }

    #[test]
    fn build_sync_adapters_filters_failing_preflight() {
        // Use a config with cloudflare adapter, but a runner that fails
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("not found")
            }
        }

        let adapters = build_sync_adapters(&config, &FailRunner);
        // env adapter has no preflight check, so it passes; cloudflare fails
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].name(), "env");
    }

    #[test]
    fn check_adapter_health_all_ok() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        struct OkRunner;
        impl CommandRunner for OkRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: b"1.0.0".to_vec(),
                    stderr: Vec::new(),
                })
            }
        }

        let health = check_adapter_health(&config, &OkRunner);
        assert_eq!(health.len(), 2);
        assert!(health[0].ok);
        assert_eq!(health[0].name, "env");
        assert!(health[1].ok);
        assert_eq!(health[1].name, "cloudflare");
    }

    #[test]
    fn check_adapter_health_cloudflare_fails() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        struct FailRunner;
        impl CommandRunner for FailRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                anyhow::bail!("not found")
            }
        }

        let health = check_adapter_health(&config, &FailRunner);
        assert_eq!(health.len(), 2);
        assert!(health[0].ok); // env always ok
        assert!(!health[1].ok); // cloudflare fails
        assert!(health[1].message.contains("wrangler is not installed"));
    }

    #[test]
    fn validate_stdin_kv_value_normal() {
        assert!(validate_stdin_kv_value("KEY", "normal_value", "test").is_ok());
    }

    #[test]
    fn validate_stdin_kv_value_rejects_newline() {
        let err = validate_stdin_kv_value("KEY", "line1\nline2", "test").unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn validate_stdin_kv_value_rejects_carriage_return() {
        let err = validate_stdin_kv_value("KEY", "line1\r\nline2", "test").unwrap_err();
        assert!(err.to_string().contains("contains newlines"));
    }

    #[test]
    fn check_adapter_health_no_adapters() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        struct OkRunner;
        impl CommandRunner for OkRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let health = check_adapter_health(&config, &OkRunner);
        assert!(health.is_empty());
    }
}
