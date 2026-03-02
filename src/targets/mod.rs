pub mod aws_lambda;
pub mod aws_ssm;
pub mod azure_app_service;
pub mod circleci;
pub mod cloudflare;
pub mod convex;
pub mod custom;
pub mod docker;
pub mod dotenv;
pub mod fly;
pub mod gcp_cloud_run;
pub mod github;
pub mod gitlab;
pub mod heroku;
pub mod kubernetes;
pub mod netlify;
pub mod railway;
pub mod render;
pub mod supabase;
pub mod vercel;

use anyhow::Result;
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use console::style;

use crate::config::{Config, ResolvedTarget};

/// Whether a deploy succeeded or failed.
pub enum DeployOutcome {
    Success,
    Failed(String),
}

impl DeployOutcome {
    pub const fn is_success(&self) -> bool {
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
    pub group: String,
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

#[derive(Debug, thiserror::Error)]
#[error("{summary}")]
pub struct CommandError {
    summary: String,
    full_stderr: String,
}

impl CommandError {
    pub fn full_stderr(&self) -> &str {
        &self.full_stderr
    }
}

fn first_line_summary(stderr: &str) -> String {
    let mut lines = stderr.lines().filter(|l| !l.trim().is_empty());
    let Some(first) = lines.next() else {
        return stderr.trim().to_string();
    };
    let rest = lines.count();
    if rest == 0 {
        first.trim().to_string()
    } else {
        format!(
            "{} ({rest} more line{})",
            first.trim(),
            if rest == 1 { "" } else { "s" }
        )
    }
}

impl CommandOutput {
    /// Check that the command succeeded, returning an error with a truncated
    /// summary of stderr. Use `CommandError::full_stderr()` to access the
    /// complete output when needed.
    pub fn check(&self, command: &str, key: &str) -> Result<()> {
        if !self.success {
            let stderr = String::from_utf8_lossy(&self.stderr);
            let summary = first_line_summary(&stderr);
            return Err(CommandError {
                summary: format!("{command} failed for {key}: {summary}"),
                full_stderr: stderr.into_owned(),
            }
            .into());
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

pub trait DeployTarget: Send + Sync {
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

    /// Whether this target passes secret values as CLI arguments (visible in `ps`).
    fn passes_value_as_cli_arg(&self) -> bool {
        false
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

/// Build common AWS CLI arguments for --region and --profile.
pub fn aws_base_args(region: Option<&str>, profile: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(r) = region {
        args.push("--region".to_string());
        args.push(r.to_string());
    }
    if let Some(p) = profile {
        args.push("--profile".to_string());
        args.push(p.to_string());
    }
    args
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

/// Health check outcome for a target or remote.
pub enum HealthStatus {
    Ok(String),
    Failed(String),
}

impl HealthStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok(_))
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Ok(msg) | Self::Failed(msg) => msg,
        }
    }
}

/// Health status of a configured target.
pub struct TargetHealth {
    pub name: String,
    pub status: HealthStatus,
}

pub(crate) struct TargetCandidate<'a> {
    pub(crate) target: Box<dyn DeployTarget + 'a>,
    pub(crate) ok_message: &'static str,
}

pub(crate) fn target_candidates<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<TargetCandidate<'a>> {
    let mut candidates: Vec<TargetCandidate<'a>> = Vec::new();

    if config.targets.dotenv.is_some() {
        candidates.push(TargetCandidate {
            target: Box::new(dotenv::DotenvTarget { config }),
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

    if let Some(target_config) = &config.targets.aws_lambda {
        candidates.push(TargetCandidate {
            target: Box::new(aws_lambda::AwsLambdaTarget {
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

    if let Some(target_config) = &config.targets.circleci {
        candidates.push(TargetCandidate {
            target: Box::new(circleci::CircleciTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "circleci available",
        });
    }

    if let Some(target_config) = &config.targets.azure_app_service {
        candidates.push(TargetCandidate {
            target: Box::new(azure_app_service::AzureAppServiceTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "az authenticated",
        });
    }

    if let Some(target_config) = &config.targets.gcp_cloud_run {
        candidates.push(TargetCandidate {
            target: Box::new(gcp_cloud_run::GcpCloudRunTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "gcloud authenticated",
        });
    }

    if let Some(target_config) = &config.targets.render {
        candidates.push(TargetCandidate {
            target: Box::new(render::RenderTarget {
                config,
                target_config,
                runner,
            }),
            ok_message: "API authenticated",
        });
    }

    for (name, target_config) in &config.targets.custom {
        candidates.push(TargetCandidate {
            target: Box::new(custom::CustomTarget {
                target_name: name.clone(),
                target_config,
                runner,
            }),
            ok_message: "ready",
        });
    }

    candidates
}

/// Check the health of all configured targets without filtering.
/// Returns one entry per configured target with preflight pass/fail.
/// Runs all preflight checks in parallel.
#[cfg(test)]
fn check_target_health(config: &Config, runner: &dyn CommandRunner) -> Vec<TargetHealth> {
    let candidates = target_candidates(config, runner);
    if candidates.is_empty() {
        return Vec::new();
    }

    let items: Vec<&dyn PreflightItem> =
        candidates.iter().map(|c| c as &dyn PreflightItem).collect();
    let results = run_preflight_section(&items, "Targets");
    candidates
        .iter()
        .zip(results)
        .map(|(c, (ok, msg))| TargetHealth {
            name: c.target.name().to_string(),
            status: if ok {
                HealthStatus::Ok(msg)
            } else {
                HealthStatus::Failed(msg)
            },
        })
        .collect()
}

/// Item that can be preflighted (targets and remotes).
pub(crate) trait PreflightItem: Send + Sync {
    fn preflight_name(&self) -> &str;
    fn preflight(&self) -> Result<()>;
    fn ok_message(&self) -> &str;
}

impl PreflightItem for TargetCandidate<'_> {
    fn preflight_name(&self) -> &str {
        self.target.name()
    }

    fn preflight(&self) -> Result<()> {
        self.target.preflight()
    }

    fn ok_message(&self) -> &str {
        self.ok_message
    }
}

/// Run preflight checks in parallel with animated rendering.
///
/// TTY path: animated spinners with diamond header using `section_name`.
/// Non-TTY path: static `cliclack::log::step` using `section_name`.
/// Returns `Vec<(bool, String)>` — one `(passed, message)` per candidate, parallel to input.
pub(crate) fn run_preflight_section(
    items: &[&dyn PreflightItem],
    section_name: &str,
) -> Vec<(bool, String)> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }

    let name_width = items
        .iter()
        .map(|c| c.preflight_name().len())
        .max()
        .unwrap_or(0)
        + 4;
    let is_tty = std::io::stderr().is_terminal();

    // Shared preflight results: None = in progress
    #[allow(clippy::type_complexity)]
    let results: Arc<Mutex<Vec<Option<(bool, String)>>>> = Arc::new(Mutex::new(vec![None; n]));

    std::thread::scope(|s| {
        // Spawn preflight workers
        for (i, c) in items.iter().enumerate() {
            let results = Arc::clone(&results);
            s.spawn(move || {
                let result = match c.preflight() {
                    Ok(()) => (true, c.ok_message().to_string()),
                    Err(e) => (false, e.to_string()),
                };
                results.lock().expect("preflight results mutex poisoned")[i] = Some(result);
            });
        }

        // Animated render loop (TTY only)
        if is_tty {
            let term = console::Term::stderr();
            let frames = crate::ui::SPINNER_FRAMES;
            let bar = style("\u{2502}").dim();

            // Print header + initial spinner lines
            let _ = term.write_line(&format!("{}  {section_name}", style("\u{25C7}").dim()));
            for c in items {
                let _ = term.write_line(&format!(
                    "{bar}    {} {:<name_width$}",
                    style(frames[0]).magenta(),
                    c.preflight_name(),
                ));
            }

            let mut frame = 0usize;
            loop {
                std::thread::sleep(crate::ui::SPINNER_INTERVAL);
                frame = (frame + 1) % frames.len();

                let state = results.lock().expect("preflight results mutex poisoned");
                let all_done = state.iter().all(Option::is_some);

                let _ = term.move_cursor_up(n);
                for (i, c) in items.iter().enumerate() {
                    let _ = term.clear_line();
                    let name = c.preflight_name();
                    match &state[i] {
                        Some((true, msg)) => {
                            let _ = term.write_line(&format!(
                                "{bar}    {} {:<name_width$}{}",
                                style("\u{2714}").green(),
                                name,
                                style(msg).dim(),
                            ));
                        }
                        Some((false, msg)) => {
                            let _ = term.write_line(&format!(
                                "{bar}    {} {:<name_width$}{}",
                                style("\u{2718}").red(),
                                name,
                                style(msg).dim(),
                            ));
                        }
                        None => {
                            let _ = term.write_line(&format!(
                                "{bar}    {} {:<name_width$}",
                                style(frames[frame]).magenta(),
                                name,
                            ));
                        }
                    }
                }

                drop(state);
                if all_done {
                    break;
                }
            }

            // Repaint header with status color
            let state = results.lock().expect("preflight results mutex poisoned");
            let all_ok = state.iter().all(|r| matches!(r, Some((true, _))));
            let any_ok = state.iter().any(|r| matches!(r, Some((true, _))));
            drop(state);
            let header_icon = if all_ok {
                style("\u{25C6}").green()
            } else if any_ok {
                style("\u{25C6}").yellow()
            } else {
                style("\u{25C6}").red()
            };
            let _ = term.move_cursor_up(n + 1);
            let _ = term.clear_line();
            let _ = term.write_line(&format!("{header_icon}  {section_name}"));
            let _ = term.move_cursor_down(n);

            // Trailing bar line to match cliclack::note spacing
            let _ = term.write_line(&format!("{bar}"));
        }
    });

    // Non-TTY fallback: static rendering via cliclack
    if !is_tty {
        let state = results.lock().expect("preflight results mutex poisoned");
        let lines: Vec<String> = items
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let name = c.preflight_name();
                let (ok, msg) = state[i].as_ref().expect("preflight result missing");
                let mark = if *ok { "\u{2714}" } else { "\u{2718}" };
                format!("  {mark} {name:<name_width$}{}", style(msg).dim())
            })
            .collect();
        let _ = cliclack::log::step(format!("{section_name}\n{}", lines.join("\n")));
    }

    // Unwrap results — all slots are filled after thread::scope completes
    let state = results.lock().expect("preflight results mutex poisoned");
    state.iter().map(|r| r.clone().expect("preflight result missing")).collect()
}

/// Render target health with animated spinners, returning health results.
///
/// Creates candidates from config, runs `run_preflight_section()` with the
/// given section name, and converts results to `Vec<TargetHealth>`.
pub fn render_target_health(
    config: &Config,
    runner: &dyn CommandRunner,
    section_name: &str,
) -> Vec<TargetHealth> {
    let candidates = target_candidates(config, runner);
    let items: Vec<&dyn PreflightItem> =
        candidates.iter().map(|c| c as &dyn PreflightItem).collect();
    let results = run_preflight_section(&items, section_name);
    candidates
        .iter()
        .zip(results)
        .map(|(c, (ok, msg))| TargetHealth {
            name: c.target.name().to_string(),
            status: if ok {
                HealthStatus::Ok(msg)
            } else {
                HealthStatus::Failed(msg)
            },
        })
        .collect()
}

/// Build all configured deploy targets from the config.
/// Runs preflight checks in parallel and filters out targets that fail.
/// Renders animated per-target spinners that resolve to checkmarks/X marks.
pub fn build_targets<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<Box<dyn DeployTarget + 'a>> {
    let candidates = target_candidates(config, runner);
    if candidates.is_empty() {
        return Vec::new();
    }

    let items: Vec<&dyn PreflightItem> =
        candidates.iter().map(|c| c as &dyn PreflightItem).collect();
    let results = run_preflight_section(&items, "Preflight");

    // Emit security warnings for passing targets
    let mut security_warnings: Vec<String> = Vec::new();
    for (i, (ok, _)) in results.iter().enumerate() {
        if *ok {
            let name = candidates[i].target.name();
            if candidates[i].target.passes_value_as_cli_arg() {
                security_warnings.push(format!(
                    "{name}: secret values are passed as CLI args and may be visible in local process listings",
                ));
            }
            if let Some(custom_cfg) = config.targets.custom.get(name) {
                if custom::has_value_in_args(&custom_cfg.deploy.args) {
                    security_warnings.push(format!(
                        "{name}: deploy args contain {{{{value}}}} — secret values will be \
                         visible in process listings. Consider using stdin instead."
                    ));
                }
            }
        }
    }

    for warning in &security_warnings {
        let _ = cliclack::log::warning(warning);
    }

    // Filter to passing targets
    candidates
        .into_iter()
        .zip(results)
        .filter_map(|(c, (ok, _))| if ok { Some(c.target) } else { None })
        .collect()
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
            if self.fail_keys.iter().any(|k| k == key) {
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
            group: "G".to_string(),
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
  .env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        let runner = ErrorCommandRunner::new("not found");
        let targets = build_targets(&config, &runner);
        // .env target has no preflight check, so it passes; cloudflare fails
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name(), ".env");
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
  .env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = crate::config::Config::load(&path).unwrap();

        let health = check_target_health(&config, &OkRunner);
        assert_eq!(health.len(), 2);
        assert!(health[0].status.is_ok());
        assert_eq!(health[0].name, ".env");
        assert!(health[1].status.is_ok());
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
  .env:
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
        assert!(health[0].status.is_ok()); // .env always ok
        assert!(!health[1].status.is_ok()); // cloudflare fails
        assert!(health[1]
            .status
            .message()
            .contains("wrangler is not installed"));
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

    #[test]
    fn first_line_summary_empty() {
        assert_eq!(first_line_summary(""), "");
        assert_eq!(first_line_summary("  \n  \n  "), "");
    }

    #[test]
    fn first_line_summary_single_line() {
        assert_eq!(first_line_summary("auth error"), "auth error");
        assert_eq!(first_line_summary("  auth error  \n"), "auth error");
    }

    #[test]
    fn first_line_summary_multi_line() {
        assert_eq!(
            first_line_summary("auth error\ndetail 1\ndetail 2"),
            "auth error (2 more lines)"
        );
        assert_eq!(
            first_line_summary("auth error\ndetail 1"),
            "auth error (1 more line)"
        );
    }

    #[test]
    fn command_error_preserves_full_stderr() {
        let output = CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"line1\nline2\nline3".to_vec(),
        };
        let err = output.check("cmd", "KEY").unwrap_err();
        let cmd_err = err.downcast_ref::<CommandError>().unwrap();
        assert_eq!(cmd_err.full_stderr(), "line1\nline2\nline3");
        assert!(cmd_err.to_string().contains("2 more lines"));
    }

    /// Verify that every name in `builtin_entries` appears in `target_candidates`
    /// when the corresponding config field is set. Catches drift between the two
    /// registration points.
    #[test]
    fn builtin_entries_matches_target_candidates() {
        use std::collections::BTreeSet;

        // Config with all 19 built-in targets enabled
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  .env:
    pattern: "{app_path}/.env"
  cloudflare:
    env_flags: {}
  convex:
    path: apps/api
  fly:
    app_names:
      web: my-fly-app
  netlify:
    env_flags: {}
  vercel:
    env_names:
      dev: development
  github:
    env_flags: {}
  heroku:
    app_names:
      web: my-heroku-app
  supabase:
    project_ref: abcdef123456
  railway:
    env_flags: {}
  gitlab:
    env_flags: {}
  aws_ssm:
    path_prefix: "/esk/{environment}/"
  aws_lambda:
    function_name:
      dev: my-lambda-fn
  kubernetes:
    secret_name: my-secret
    namespace:
      dev: default
  docker:
    env_flags: {}
  circleci:
    org_id: org-xxx
    context_name: my-ctx
  azure_app_service:
    app_names:
      web: my-azure-app
    resource_group: rg
  gcp_cloud_run:
    service_names:
      web: my-svc
    project: my-gcp-project
    region: us-central1
  render:
    service_ids:
      web: srv-xxx
"#;
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

        let candidates = target_candidates(&config, &OkRunner);
        let candidate_names: BTreeSet<&str> =
            candidates.iter().map(|c| c.target.name()).collect();

        let entry_names: BTreeSet<&str> = config
            .targets
            .builtin_entries()
            .iter()
            .map(|(name, _)| *name)
            .collect();

        assert_eq!(
            entry_names, candidate_names,
            "builtin_entries() and target_candidates() have drifted: \
             in entries but not candidates: {:?}, \
             in candidates but not entries: {:?}",
            entry_names.difference(&candidate_names).collect::<Vec<_>>(),
            candidate_names.difference(&entry_names).collect::<Vec<_>>(),
        );
    }
}
