//! Docker Swarm target — deploys secrets via the `docker` CLI.
//!
//! Docker Swarm secrets are encrypted at rest in the Raft log and mounted as
//! tmpfs files at `/run/secrets/<name>` inside containers — never exposed as
//! environment variables or CLI arguments.
//!
//! CLI: `docker` (Docker Engine CLI).
//! Commands: `docker secret create <name> -` (set, stdin) / `docker secret rm <name>` (delete).
//!
//! Operates in **individual mode**: each esk secret maps to a named Docker
//! secret. Since Docker secrets are immutable, updates require a remove-then-
//! recreate cycle. Values are passed via stdin for security. A configurable
//! `name_pattern` with `{project}`, `{environment}`, `{key}` placeholders
//! prevents naming collisions across environments.

use anyhow::{Context, Result};

use crate::config::{Config, DockerTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

pub struct DockerTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a DockerTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DockerTarget<'_> {
    fn resolve_name(&self, key: &str, target: &ResolvedTarget) -> String {
        self.target_config
            .name_pattern
            .replace("{project}", &self.config.project)
            .replace("{environment}", &target.environment)
            .replace("{key}", key)
    }

    fn label_args(&self) -> Vec<String> {
        self.target_config
            .labels
            .iter()
            .flat_map(|(k, v)| vec!["--label".to_string(), format!("{k}={v}")])
            .collect()
    }
}

impl DeployTarget for DockerTarget<'_> {
    fn name(&self) -> &'static str {
        "docker"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "docker").map_err(|_| {
            anyhow::anyhow!(
                "docker is not installed or not in PATH. Install it from: https://docs.docker.com/get-docker/"
            )
        })?;

        let output = self
            .runner
            .run(
                "docker",
                &["info", "--format", "{{.Swarm.LocalNodeState}}"],
                CommandOpts::default(),
            )
            .context("failed to run docker info")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("docker daemon is not running: {stderr}");
        }

        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if state != "active" {
            anyhow::bail!(
                "docker swarm mode is not active (state: {state}). Run: docker swarm init"
            );
        }

        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let resolved_name = self.resolve_name(key, target);
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);

        // Remove existing secret (tolerate "no such secret" for first-time creates)
        let mut rm_args: Vec<&str> = vec!["secret", "rm", &resolved_name];
        rm_args.extend(flag_parts.iter().map(String::as_str));

        let rm_output = self
            .runner
            .run("docker", &rm_args, CommandOpts::default())
            .with_context(|| format!("failed to run docker secret rm for {key}"))?;

        if !rm_output.success {
            let stderr = String::from_utf8_lossy(&rm_output.stderr);
            let stderr_lower = stderr.to_lowercase();
            if !stderr_lower.contains("no such secret") && !stderr_lower.contains("not found") {
                anyhow::bail!("docker secret rm failed for {key}: {stderr}");
            }
        }

        // Create secret via stdin
        let label_parts = self.label_args();
        let mut create_args: Vec<&str> = vec!["secret", "create"];
        create_args.extend(label_parts.iter().map(String::as_str));
        create_args.push(&resolved_name);
        create_args.push("-");
        create_args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run(
                "docker",
                &create_args,
                CommandOpts {
                    stdin: Some(value.as_bytes().to_vec()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run docker secret create for {key}"))?;

        output.check("docker secret create", key)
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let resolved_name = self.resolve_name(key, target);
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);

        let mut args: Vec<&str> = vec!["secret", "rm", &resolved_name];
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("docker", &args, CommandOpts::default())
            .with_context(|| format!("failed to run docker secret rm for {key}"))?
            .check("docker secret rm", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
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
project: myapp
environments: [dev, prod]
targets:
  docker:
    name_pattern: "{project}-{environment}-{key}"
    labels:
      managed-by: esk
    env_flags:
      prod: "--context prod-swarm"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "docker".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"Docker version 24.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"active\n".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = take_calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(
            calls[1].1,
            vec!["info", "--format", "{{.Swarm.LocalNodeState}}"]
        );
    }

    #[test]
    fn preflight_missing_cli() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("docker is not installed"));
    }

    #[test]
    fn preflight_daemon_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"Docker version 24.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"Cannot connect to the Docker daemon".to_vec(),
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("docker daemon is not running"));
    }

    #[test]
    fn preflight_swarm_not_active() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"Docker version 24.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"inactive".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("swarm mode is not active"));
    }

    #[test]
    fn deploy_creates_via_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // rm returns "no such secret" (first-time create)
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"Error: No such secret: myapp-dev-API_KEY".to_vec(),
            },
            // create succeeds
            CommandOutput {
                success: true,
                stdout: b"secret-id-123".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("API_KEY", "s3cret", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls.len(), 2);
        // rm call
        assert_eq!(calls[0].0, "docker");
        assert_eq!(calls[0].1, vec!["secret", "rm", "myapp-dev-API_KEY"]);
        // create call — value via stdin, not in args
        assert_eq!(calls[1].0, "docker");
        assert!(calls[1].1.contains(&"secret".to_string()));
        assert!(calls[1].1.contains(&"create".to_string()));
        assert!(calls[1].1.contains(&"myapp-dev-API_KEY".to_string()));
        assert_eq!(calls[1].2.as_deref(), Some(b"s3cret".as_slice()));
    }

    #[test]
    fn deploy_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // rm succeeds (secret existed)
            CommandOutput {
                success: true,
                stdout: b"myapp-dev-API_KEY".to_vec(),
                stderr: vec![],
            },
            // create succeeds
            CommandOutput {
                success: true,
                stdout: b"secret-id-456".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("API_KEY", "new_val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["secret", "rm", "myapp-dev-API_KEY"]);
        assert!(calls[1].1.contains(&"create".to_string()));
    }

    #[test]
    fn deploy_rm_fails_service_in_use() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // rm fails with "secret is in use"
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"Error response from daemon: secret is in use by service".to_vec(),
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("API_KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("secret rm failed"));
        // Should not have attempted create
        let calls = take_calls(&runner);
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn deploy_create_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // rm tolerated (no such secret)
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"No such secret".to_vec(),
            },
            // create fails
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"permission denied".to_vec(),
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("API_KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("secret create failed"));
    }

    #[test]
    fn deploy_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // rm (no such secret)
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"No such secret".to_vec(),
            },
            // create succeeds
            CommandOutput {
                success: true,
                stdout: vec![],
                stderr: vec![],
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = take_calls(&runner);
        // rm call should have env_flags
        assert!(calls[0].1.contains(&"--context".to_string()));
        assert!(calls[0].1.contains(&"prod-swarm".to_string()));
        // create call should have env_flags
        assert!(calls[1].1.contains(&"--context".to_string()));
        assert!(calls[1].1.contains(&"prod-swarm".to_string()));
    }

    #[test]
    fn deploy_with_labels() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // rm (no such secret)
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"No such secret".to_vec(),
            },
            // create succeeds
            CommandOutput {
                success: true,
                stdout: vec![],
                stderr: vec![],
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        // create call should have --label
        assert!(calls[1].1.contains(&"--label".to_string()));
        assert!(calls[1].1.contains(&"managed-by=esk".to_string()));
    }

    #[test]
    fn deploy_value_not_in_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"No such secret".to_vec(),
            },
            CommandOutput {
                success: true,
                stdout: vec![],
                stderr: vec![],
            },
        ]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "super_secret_value", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        // Value must never appear in args of any call
        for call in &calls {
            assert!(
                !call.1.iter().any(|a| a.contains("super_secret_value")),
                "secret value leaked into CLI args"
            );
        }
        // Value should be in stdin of create call
        assert_eq!(
            calls[1].2.as_deref(),
            Some(b"super_secret_value".as_slice())
        );
    }

    #[test]
    fn resolve_name_default_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &MockCommandRunner::new(),
        };
        let name = target.resolve_name("API_KEY", &make_target("dev"));
        assert_eq!(name, "myapp-dev-API_KEY");
    }

    #[test]
    fn resolve_name_custom_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: myapp
environments: [dev]
targets:
  docker:
    name_pattern: "{environment}/{project}/{key}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let target_config = config.targets.docker.as_ref().unwrap();
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &MockCommandRunner::new(),
        };
        let name = target.resolve_name("DB_URL", &make_target("dev"));
        assert_eq!(name, "dev/myapp/DB_URL");
    }

    #[test]
    fn delete_correct_args() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("API_KEY", &make_target("dev"))
            .unwrap();
        let calls = take_calls(&runner);
        assert_eq!(calls[0].0, "docker");
        assert_eq!(calls[0].1, vec!["secret", "rm", "myapp-dev-API_KEY"]);
    }

    #[test]
    fn delete_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"secret is in use".to_vec(),
        }]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("API_KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("secret rm failed"));
    }

    #[test]
    fn delete_with_env_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config(dir.path());
        let target_config = config.targets.docker.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("KEY", &make_target("prod")).unwrap();
        let calls = take_calls(&runner);
        assert!(calls[0].1.contains(&"--context".to_string()));
        assert!(calls[0].1.contains(&"prod-swarm".to_string()));
    }

    #[test]
    fn default_name_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: myapp
environments: [dev]
targets:
  docker: {}
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let target_config = config.targets.docker.as_ref().unwrap();
        assert_eq!(target_config.name_pattern, "{project}-{environment}-{key}");

        let target = DockerTarget {
            config: &config,
            target_config,
            runner: &MockCommandRunner::new(),
        };
        let name = target.resolve_name("SECRET", &make_target("dev"));
        assert_eq!(name, "myapp-dev-SECRET");
    }
}
