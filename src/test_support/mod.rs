use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

use crate::targets::{CommandOpts, CommandOutput, CommandRunner};
use crate::config::Config;

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
    Error(String),
}

/// Shared command runner test double for target and remote unit tests.
pub struct MockCommandRunner {
    calls: Mutex<Vec<RecordedCall>>,
    responses: Mutex<Vec<QueuedResponse>>,
}

impl MockCommandRunner {
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responses: Mutex::new(Vec::new()),
        }
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

    pub fn push_error(&self, message: impl Into<String>) {
        self.responses
            .lock()
            .expect("runner responses mutex poisoned")
            .push(QueuedResponse::Error(message.into()));
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
                args: args.iter().map(|s| s.to_string()).collect(),
                cwd: opts.cwd,
                stdin: opts.stdin,
                env: opts.env,
            });

        let mut responses = self
            .responses
            .lock()
            .expect("runner responses mutex poisoned");
        if responses.is_empty() {
            return Ok(CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            });
        }

        match responses.remove(0) {
            QueuedResponse::Output(output) => Ok(output),
            QueuedResponse::Error(message) => Err(anyhow!(message)),
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

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    pub fn create_dir_all(&self, relative: &str) -> Result<PathBuf> {
        let path = self.path(relative);
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    pub fn write(&self, relative: &str, contents: impl AsRef<[u8]>) -> Result<PathBuf> {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, contents)?;
        Ok(path)
    }
}
