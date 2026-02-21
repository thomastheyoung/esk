pub mod cloudflare;
pub mod convex;
pub mod env_file;

use anyhow::Result;
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

    /// Sync a single secret to a target.
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;

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

/// Build all configured sync adapters from the config.
pub fn build_sync_adapters<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<Box<dyn SyncAdapter + 'a>> {
    let mut adapters: Vec<Box<dyn SyncAdapter + 'a>> = Vec::new();

    if config.adapters.env.is_some() {
        adapters.push(Box::new(env_file::EnvFileAdapter { config }));
    }

    if let Some(adapter_config) = &config.adapters.cloudflare {
        adapters.push(Box::new(cloudflare::CloudflareAdapter {
            config,
            adapter_config,
            runner,
        }));
    }

    if let Some(adapter_config) = &config.adapters.convex {
        adapters.push(Box::new(convex::ConvexAdapter {
            config,
            adapter_config,
            runner,
        }));
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
}
