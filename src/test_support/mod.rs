use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::sync::Mutex;
use tempfile::TempDir;

use crate::config::Config;
use crate::targets::{CommandOpts, CommandOutput, CommandRunner};

#[derive(Debug, Clone)]
pub struct RecordedCall {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    pub env: Vec<(String, String)>,
}

enum QueuedResponse {
    Output(CommandOutput),
}

/// Shared command runner test double for target and remote unit tests.
pub struct MockCommandRunner {
    calls: Mutex<Vec<RecordedCall>>,
    responses: Mutex<Vec<QueuedResponse>>,
    strict: bool,
}

impl MockCommandRunner {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(Vec::new()),
            strict: false,
        }
    }

    /// Panic instead of inventing a success when the response queue is empty.
    ///
    /// The lenient default suits tests that only assert on recorded calls. Use
    /// this whenever a test asserts an exact call count or indexes into
    /// `calls`: an unqueued call would otherwise be served a synthetic success
    /// with empty stdout, which parsers read as "no data" and tests read as
    /// green. Strict mode turns that silent corruption into a named failure.
    ///
    /// Strictness is opt-in rather than automatic on first `push_*`. Queuing a
    /// response does not imply the test cares about every later one: the
    /// `deploy_prune_*` and `status_shows_target_orphans` tests deliberately
    /// queue the deploy phase and let the prune phase's calls fall through,
    /// asserting only that deletion was attempted. Inferring strictness from
    /// queue use breaks all seven.
    #[must_use]
    pub fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    pub fn from_outputs(outputs: Vec<CommandOutput>) -> Self {
        let runner = Self::new();
        for output in outputs {
            runner.push_output(output);
        }
        runner
    }

    pub fn push_output(&self, output: CommandOutput) {
        self.responses
            .lock()
            .expect("runner responses mutex poisoned")
            .push(QueuedResponse::Output(output));
    }

    pub fn push_success(&self, stdout: &[u8], stderr: &[u8]) {
        self.push_output(CommandOutput {
            success: true,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        });
    }

    pub fn push_failure(&self, stderr: &[u8]) {
        self.push_output(CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        });
    }

    pub fn take_calls(&self) -> Vec<RecordedCall> {
        std::mem::take(&mut *self.calls.lock().expect("runner calls mutex poisoned"))
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.calls
            .lock()
            .expect("runner calls mutex poisoned")
            .clone()
    }
}

impl Default for MockCommandRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandRunner for MockCommandRunner {
    fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput> {
        self.calls
            .lock()
            .expect("runner calls mutex poisoned")
            .push(RecordedCall {
                program: program.to_string(),
                args: args.iter().map(|s| (*s).to_string()).collect(),
                cwd: opts.cwd,
                stdin: opts.stdin,
                env: opts.env,
            });

        let mut responses = self
            .responses
            .lock()
            .expect("runner responses mutex poisoned");
        if responses.is_empty() {
            assert!(
                !self.strict,
                "MockCommandRunner (strict): no queued response for `{program} {}`",
                args.join(" ")
            );
            return Ok(CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }

        match responses.remove(0) {
            QueuedResponse::Output(output) => Ok(output),
        }
    }
}

/// Command runner that always fails with the configured message.
pub struct ErrorCommandRunner {
    message: String,
}

impl ErrorCommandRunner {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn missing_command() -> Self {
        Self::new("No such file or directory")
    }
}

impl CommandRunner for ErrorCommandRunner {
    fn run(&self, _program: &str, _args: &[&str], _opts: CommandOpts) -> Result<CommandOutput> {
        Err(anyhow!(self.message.clone()))
    }
}

/// Keeps a loaded `Config` and its temporary project directory alive together.
pub struct ConfigFixture {
    dir: TempDir,
    config: Config,
}

impl ConfigFixture {
    pub fn new(yaml: &str) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml)?;
        let config = Config::load(&path)?;
        Ok(Self { dir, config })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Create a subdirectory tree under the fixture root.
    pub fn create_dir_all(&self, relative: &str) -> Result<()> {
        let p = self.dir.path().join(relative);
        std::fs::create_dir_all(&p)?;
        Ok(())
    }

    /// Return the absolute path for a relative path under the fixture root.
    pub fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }
}

#[cfg(test)]
mod strict_tests {
    use super::*;

    #[test]
    fn lenient_mode_invents_success_when_queue_is_empty() {
        // Documents the default behaviour that strict mode exists to guard
        // against: an unqueued call is served a synthetic success.
        let runner = MockCommandRunner::new();
        let out = runner.run("wrangler", &["secret", "list"], CommandOpts::default());
        let out = out.expect("lenient mode returns a synthetic success");
        assert!(out.success);
        assert!(out.stdout.is_empty());
    }

    #[test]
    fn strict_mode_serves_queued_responses_normally() {
        let runner = MockCommandRunner::new().strict();
        runner.push_success(b"payload", b"");
        let out = runner
            .run("wrangler", &["secret", "list"], CommandOpts::default())
            .expect("queued response is returned");
        assert!(out.success);
        assert_eq!(out.stdout, b"payload");
    }

    #[test]
    #[should_panic(expected = "no queued response for `wrangler secret list`")]
    fn strict_mode_panics_when_queue_is_exhausted() {
        let runner = MockCommandRunner::new().strict();
        runner.push_success(b"first", b"");
        let _ = runner.run("wrangler", &["secret", "put"], CommandOpts::default());
        // Second call has nothing queued: strict mode must name the offender
        // rather than inventing a success.
        let _ = runner.run("wrangler", &["secret", "list"], CommandOpts::default());
    }
}
