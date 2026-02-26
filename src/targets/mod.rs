pub mod aws_ssm;
pub mod cloudflare;
pub mod convex;
pub mod docker;
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

/// Whether a deploy succeeded or failed.
pub enum DeployOutcome {
    Success,
    Failed(String),
}

impl DeployOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    pub fn error_message(&self) -> Option<&str> {
        match self {
            Self::Success => None,
            Self::Failed(e) => Some(e),
        }
    }
}

pub struct DeployResult {
    pub key: String,
    pub outcome: DeployOutcome,
}

/// Secret with its key and value, ready for deploying.
#[derive(Clone)]
pub struct SecretValue {
    pub key: String,
    pub value: String,
    pub vendor: String,
}

/// Whether a target deploys secrets individually or as a batch per target group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeployMode {
    /// Deploy each secret individually (e.g. cloudflare, convex).
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

impl CommandOutput {
    /// Check that the command succeeded, returning an error with stderr if it failed.
    pub fn check(&self, command: &str, key: &str) -> Result<()> {
        if !self.success {
            let stderr = String::from_utf8_lossy(&self.stderr);
            anyhow::bail!("{command} failed for {key}: {stderr}");
        }
        Ok(())
    }
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

pub trait DeployTarget {
    fn name(&self) -> &str;

    /// Whether this target deploys individually or in batches.
    fn deploy_mode(&self) -> DeployMode;

    /// Validate that external dependencies are available before deploying.
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Deploy a single secret to a target.
    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;

    /// Delete a single secret from a target. Default: no-op (batch targets handle deletion
    /// by regenerating the full output without the deleted key).
    fn delete_secret(&self, _key: &str, _target: &ResolvedTarget) -> Result<()> {
        Ok(())
    }

    /// Deploy a batch of secrets. Default implementation loops deploy_secret.
    fn deploy_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<DeployResult> {
        secrets
            .iter()
            .map(|s| match self.deploy_secret(&s.key, &s.value, target) {
                Ok(()) => DeployResult {
                    key: s.key.clone(),
                    outcome: DeployOutcome::Success,
                },
                Err(e) => DeployResult {
                    key: s.key.clone(),
                    outcome: DeployOutcome::Failed(e.to_string()),
                },
            })
            .collect()
    }
}

/// Validate that a secret value is safe for stdin-based KEY=VALUE targets.
///
/// Targets like Fly and Supabase pass `KEY=VALUE\n` via stdin. A newline
/// in the value would inject additional environment variables.
pub fn validate_stdin_kv_value(key: &str, value: &str, target_name: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!(
            "{target_name}: secret '{key}' contains newlines, which would inject \
             additional variables via stdin. Remove newlines or use a different target."
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

/// Check that an external command is available via the CommandRunner.
pub fn check_command(runner: &dyn CommandRunner, program: &str) -> Result<()> {
    runner
        .run(program, &["--version"], CommandOpts::default())
        .map_err(|_| {
            anyhow::anyhow!("{program} is not installed or not in PATH. Install it and try again.")
        })?;
    Ok(())
}

/// Health status of a configured target.
pub struct TargetHealth {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

struct TargetCandidate<'a> {
    target: Box<dyn DeployTarget + 'a>,
    ok_message: &'static str,
}

fn target_candidates<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<TargetCandidate<'a>> {
    let mut candidates: Vec<TargetCandidate<'a>> = Vec::new();

    if config.targets.env.is_some() {
        candidates.push(TargetCandidate {
            target: Box::new(env_file::EnvFileTarget { config }),
            ok_message: "writable",
        });
    }

    if let Some(target_config) = &config.targets.cloudflare {
        candidates.push(TargetCandidate {
            target: Box::new(cloudflare::CloudflareTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "wrangler authenticated",
        });
    }

    if let Some(target_config) = &config.targets.convex {
        candidates.push(TargetCandidate {
            target: Box::new(convex::ConvexTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "deployment accessible",
        });
    }

    if let Some(target_config) = &config.targets.fly {
        candidates.push(TargetCandidate {
            target: Box::new(fly::FlyTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "fly authenticated",
        });
    }

    if let Some(target_config) = &config.targets.netlify {
        candidates.push(TargetCandidate {
            target: Box::new(netlify::NetlifyTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "netlify linked",
        });
    }

    if let Some(target_config) = &config.targets.vercel {
        candidates.push(TargetCandidate {
            target: Box::new(vercel::VercelTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "vercel authenticated",
        });
    }

    if let Some(target_config) = &config.targets.github {
        candidates.push(TargetCandidate {
            target: Box::new(github::GithubTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "gh authenticated",
        });
    }

    if let Some(target_config) = &config.targets.heroku {
        candidates.push(TargetCandidate {
            target: Box::new(heroku::HerokuTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "heroku authenticated",
        });
    }

    if let Some(target_config) = &config.targets.supabase {
        candidates.push(TargetCandidate {
            target: Box::new(supabase::SupabaseTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "supabase available",
        });
    }

    if let Some(target_config) = &config.targets.railway {
        candidates.push(TargetCandidate {
            target: Box::new(railway::RailwayTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "railway authenticated",
        });
    }

    if let Some(target_config) = &config.targets.gitlab {
        candidates.push(TargetCandidate {
            target: Box::new(gitlab::GitlabTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "glab authenticated",
        });
    }

    if let Some(target_config) = &config.targets.aws_ssm {
        candidates.push(TargetCandidate {
            target: Box::new(aws_ssm::AwsSsmTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "aws authenticated",
        });
    }

    if let Some(target_config) = &config.targets.kubernetes {
        candidates.push(TargetCandidate {
            target: Box::new(kubernetes::KubernetesTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "kubectl available",
        });
    }

    if let Some(target_config) = &config.targets.docker {
        candidates.push(TargetCandidate {
            target: Box::new(docker::DockerTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "swarm active",
        });
    }

    candidates
}

fn needs_cli_secret_arg_warning(name: &str) -> bool {
    matches!(name, "convex" | "netlify" | "heroku" | "railway")
}

/// Check the health of all configured targets without filtering.
/// Returns one entry per configured target with preflight pass/fail.
pub fn check_target_health(config: &Config, runner: &dyn CommandRunner) -> Vec<TargetHealth> {
    let mut health = Vec::new();
    for candidate in target_candidates(config, runner) {
        let name = candidate.target.name().to_string();
        match candidate.target.preflight() {
            Ok(()) => health.push(TargetHealth {
                name,
                ok: true,
                message: candidate.ok_message.to_string(),
            }),
            Err(e) => health.push(TargetHealth {
                name,
                ok: false,
                message: e.to_string(),
            }),
        }
    }
    health
}

/// Build all configured deploy targets from the config.
/// Runs preflight checks and filters out targets that fail, printing warnings.
pub fn build_targets<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<Box<dyn DeployTarget + 'a>> {
    let mut targets: Vec<Box<dyn DeployTarget + 'a>> = Vec::new();

    for candidate in target_candidates(config, runner) {
        let target = candidate.target;
        match target.preflight() {
            Ok(()) => {
                if needs_cli_secret_arg_warning(target.name()) {
                    let _ = cliclack::log::warning(format!(
                        "{}: security note: secret values are passed as CLI args and may be visible in local process listings",
                        target.name()
                    ));
                }
                targets.push(target);
            }
            Err(e) => {
                let _ = cliclack::log::warning(format!("Skipping {} target: {}", target.name(), e));
            }
        }
    }

    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ErrorCommandRunner;

    struct TestTarget {
        fail_keys: Vec<String>,
    }

    impl DeployTarget for TestTarget {
        fn name(&self) -> &'static str {
            "test"
        }

        fn deploy_mode(&self) -> DeployMode {
            DeployMode::Individual
        }

        fn deploy_secret(&self, key: &str, _value: &str, _target: &ResolvedTarget) -> Result<()> {
            if self.fail_keys.contains(&key.to_string()) {
                anyhow::bail!("deploy failed for {key}");
            }
            Ok(())
        }
    }

    fn make_target() -> ResolvedTarget {
        ResolvedTarget {
            service: "test".to_string(),
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
    fn default_deploy_batch_all_success() {
        let target = TestTarget { fail_keys: vec![] };
        let secrets = vec![make_secret("A"), make_secret("B")];
        let results = target.deploy_batch(&secrets, &make_target());
        assert!(results.iter().all(|r| r.outcome.is_success()));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn default_deploy_batch_partial_failure() {
        let target = TestTarget {
            fail_keys: vec!["B".to_string()],
        };
        let secrets = vec![make_secret("A"), make_secret("B"), make_secret("C")];
        let results = target.deploy_batch(&secrets, &make_target());
        assert!(results[0].outcome.is_success());
        assert!(!results[1].outcome.is_success());
        assert!(results[1].outcome.error_message().is_some());
        assert!(results[2].outcome.is_success());
    }

    #[test]
    fn default_deploy_batch_empty_input() {
        let target = TestTarget { fail_keys: vec![] };
        let results = target.deploy_batch(&[], &make_target());
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
        let runner = ErrorCommandRunner::missing_command();
        let err = check_command(&runner, "missing-tool").unwrap_err();
        assert!(err.to_string().contains("missing-tool is not installed"));
    }

    #[test]
    fn build_targets_filters_failing_preflight() {
        // Use a config with cloudflare target, but a runner that fails
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        let runner = ErrorCommandRunner::new("not found");
        let targets = build_targets(&config, &runner);
        // env target has no preflight check, so it passes; cloudflare fails
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name(), "env");
    }

    #[test]
    fn check_target_health_all_ok() {
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

        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        let health = check_target_health(&config, &OkRunner);
        assert_eq!(health.len(), 2);
        assert!(health[0].ok);
        assert_eq!(health[0].name, "env");
        assert!(health[1].ok);
        assert_eq!(health[1].name, "cloudflare");
    }

    #[test]
    fn check_target_health_cloudflare_fails() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        let runner = ErrorCommandRunner::new("not found");
        let health = check_target_health(&config, &runner);
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
    fn check_target_health_no_targets() {
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

        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        let health = check_target_health(&config, &OkRunner);
        assert!(health.is_empty());
    }
}
