//! Infisical remote — syncs secrets via the `infisical` CLI.
//!
//! Infisical is an open-source secrets management platform. Secrets are
//! organized into projects, environments (slugs), and folder paths.
//!
//! CLI: `infisical` (Infisical's official CLI).
//! Commands: `infisical secrets set --file=<path>` / `infisical export --format=json`.
//!
//! Push uses a temp file in `.env` format (`KEY=VALUE\n`) with `secrets set --file`.
//! Because `secrets set --file` is **upsert-only** (does not delete absent keys),
//! push first exports the current remote state, diffs it, and explicitly deletes
//! orphaned keys via `infisical secrets delete`.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::io::Write;

use crate::config::{Config, InfisicalRemoteConfig};
use crate::store::StorePayload;
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct InfisicalRemote<'a> {
    remote_config: InfisicalRemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> InfisicalRemote<'a> {
    pub fn new(remote_config: InfisicalRemoteConfig, runner: &'a dyn CommandRunner) -> Self {
        Self {
            remote_config,
            runner,
        }
    }

    /// Resolve the Infisical environment slug for an esk environment.
    fn env_slug(&self, env: &str) -> String {
        self.remote_config
            .env_map
            .get(env)
            .cloned()
            .unwrap_or_else(|| env.to_string())
    }

    /// Build the common CLI args shared across push/pull/delete calls.
    fn base_args(&self, slug: &str) -> Vec<String> {
        vec![
            "--projectId".to_string(),
            self.remote_config.project_id.clone(),
            "--env".to_string(),
            slug.to_string(),
            "--path".to_string(),
            self.remote_config.path.clone(),
        ]
    }
}

/// Surface a non-fatal degradation of orphan detection during `push()`.
///
/// `push()` runs on worker threads inside `std::thread::scope` while a
/// `cliclack` spinner is active on the main thread (see `cli/sync.rs`).
/// `cliclack` renders to stdout, redrawing the spinner line in place, so any
/// concurrent write to stdout from another thread corrupts that rendering.
/// Writing to stderr instead uses a separate stream that `cliclack` never
/// touches, so it cannot interleave with or corrupt the spinner — this
/// mirrors `main.rs`'s own top-level error reporting, which also writes
/// terminal-facing diagnostics via `eprintln!`.
///
/// This message is deliberately best-effort and non-blocking: skipping the
/// orphan check is not itself an error (the push still proceeds and can
/// still succeed), so this must never be promoted to `Result` — doing so
/// would make a transient export failure abort the `secrets set` call that
/// follows, which is strictly worse than today's silent skip.
fn warn_orphan_check_skipped(message: &str) {
    #[cfg(test)]
    {
        test_warnings::record(message);
    }
    eprintln!("esk: warning: infisical: {message}");
}

/// Test-only capture of warnings emitted via [`warn_orphan_check_skipped`].
///
/// `eprintln!` writes to the process's real stderr, which unit tests cannot
/// portably or safely intercept (fd redirection is process-global and races
/// under the default parallel test runner). This thread-local sink lets
/// tests observe *whether and what* was warned without touching fd 2 or
/// adding a capture dependency.
#[cfg(test)]
mod test_warnings {
    use std::cell::RefCell;

    thread_local! {
        static CAPTURED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    pub fn record(message: &str) {
        CAPTURED.with(|c| c.borrow_mut().push(message.to_string()));
    }

    /// Clear this thread's captured warnings and return what was captured.
    pub fn take() -> Vec<String> {
        CAPTURED.with(|c| std::mem::take(&mut *c.borrow_mut()))
    }
}

/// Parse Infisical's JSON export format (array of objects) into a flat key→value map.
///
/// Infisical exports: `[{"key":"K","value":"V","type":"shared",...}, ...]`
fn parse_export_json(stdout: &[u8]) -> Result<BTreeMap<String, String>> {
    let entries: Vec<serde_json::Value> =
        serde_json::from_slice(stdout).context("failed to parse Infisical export JSON")?;
    let mut map = BTreeMap::new();
    for entry in entries {
        let key = entry
            .get("key")
            .and_then(|v| v.as_str())
            .context("Infisical export entry missing 'key' field")?;
        let value = entry.get("value").and_then(|v| v.as_str()).unwrap_or("");
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

impl SyncRemote for InfisicalRemote<'_> {
    fn name(&self) -> &'static str {
        "infisical"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "infisical")
            .context("Install from: https://infisical.com/docs/cli/overview")?;
        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        let Some(push_map) = super::flat_snapshot_map(payload, env)? else {
            return Ok(());
        };

        let slug = self.env_slug(env);
        let base = self.base_args(&slug);

        // Build the push map: bare keys + version metadata
        // Delete orphaned keys: export current remote state, diff, delete absent keys.
        // If the export fails (empty project, etc.), skip deletion and proceed with set.
        let mut export_args = vec!["export", "--format", "json"];
        export_args.extend(base.iter().map(String::as_str));

        match self
            .runner
            .run("infisical", &export_args, CommandOpts::default())
        {
            Err(e) => {
                warn_orphan_check_skipped(&format!(
                    "could not run `infisical export` to check for orphaned keys: {e}"
                ));
            }
            Ok(output) if !output.success => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                warn_orphan_check_skipped(&format!(
                    "`infisical export` exited with an error, so orphaned keys were not checked: {stderr}"
                ));
            }
            Ok(output) => match parse_export_json(&output.stdout) {
                Err(e) => {
                    warn_orphan_check_skipped(&format!(
                        "could not parse `infisical export` output, so orphaned keys were not checked: {e}"
                    ));
                }
                Ok(remote_keys) => {
                    let orphans: Vec<&str> = remote_keys
                        .keys()
                        .filter(|k| !push_map.contains_key(k.as_str()))
                        .map(String::as_str)
                        .collect();

                    if !orphans.is_empty() {
                        let mut delete_args = vec!["secrets", "delete"];
                        delete_args.extend(orphans);
                        delete_args.extend(base.iter().map(String::as_str));

                        let del_output = self
                            .runner
                            .run("infisical", &delete_args, CommandOpts::default())
                            .context("failed to run infisical secrets delete")?;
                        if !del_output.success {
                            let stderr = String::from_utf8_lossy(&del_output.stderr);
                            anyhow::bail!("infisical secrets delete failed: {stderr}");
                        }
                    }
                }
            },
        }

        // Write secrets to a temp file in .env format and push via --file
        let mut tmpfile =
            tempfile::NamedTempFile::new().context("failed to create temp file for push")?;
        for (key, value) in &push_map {
            writeln!(tmpfile, "{key}={value}").context("failed to write to temp file")?;
        }
        tmpfile.flush().context("failed to flush temp file")?;

        let tmppath = tmpfile.path().to_string_lossy().to_string();
        let file_arg = format!("--file={tmppath}");
        let mut set_args = vec!["secrets", "set", &file_arg, "--silent"];
        set_args.extend(base.iter().map(String::as_str));

        let output = self
            .runner
            .run("infisical", &set_args, CommandOpts::default())
            .context("failed to run infisical secrets set")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("infisical secrets set failed: {stderr}");
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<super::RemoteSnapshot>> {
        let slug = self.env_slug(env);
        let base = self.base_args(&slug);

        let mut args = vec!["export", "--format", "json"];
        args.extend(base.iter().map(String::as_str));

        let output = self
            .runner
            .run("infisical", &args, CommandOpts::default())
            .context("failed to run infisical export")?;

        if !output.success {
            anyhow::bail!("infisical export failed");
        }

        let data = parse_export_json(&output.stdout)?;
        Ok(Some(super::parse_flat_snapshot(data, env)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_remote(runner: &dyn CommandRunner) -> InfisicalRemote<'_> {
        InfisicalRemote::new(
            InfisicalRemoteConfig {
                project_id: "proj123".to_string(),
                env_map: {
                    let mut m = BTreeMap::new();
                    m.insert("dev".to_string(), "development".to_string());
                    m.insert("prod".to_string(), "production".to_string());
                    m
                },
                path: "/".to_string(),
            },
            runner,
        )
    }

    fn make_payload(secrets: &[(&str, &str)], version: u64) -> StorePayload {
        let mut map = BTreeMap::new();
        for (k, v) in secrets {
            map.insert((*k).to_string(), (*v).to_string());
        }
        StorePayload {
            secrets: map,
            version,
            ..Default::default()
        }
    }

    fn make_config() -> ConfigFixture {
        ConfigFixture::new(
            r"
project: myapp
environments: [dev, prod]
remotes:
  infisical:
    project_id: proj123
    env_map:
      dev: development
      prod: production
",
        )
        .unwrap()
    }

    /// Runner whose first call errors at the spawn level (e.g. missing CLI),
    /// then delegates every subsequent call to `inner`.
    ///
    /// `MockCommandRunner` cannot make only its first call error, and
    /// `ErrorCommandRunner` fails every call — neither can express "the
    /// export call fails to spawn, but `secrets set` still runs normally".
    struct FirstCallErrorsRunner {
        inner: MockCommandRunner,
        calls: std::sync::Mutex<u32>,
    }
    impl CommandRunner for FirstCallErrorsRunner {
        fn run(&self, program: &str, args: &[&str], opts: CommandOpts) -> Result<CommandOutput> {
            let mut n = self
                .calls
                .lock()
                .expect("FirstCallErrorsRunner calls mutex poisoned");
            *n += 1;
            if *n == 1 {
                return Err(anyhow::anyhow!("No such file or directory"));
            }
            self.inner.run(program, args, opts)
        }
    }

    fn export_json(entries: &[(&str, &str)]) -> Vec<u8> {
        let arr: Vec<serde_json::Value> = entries
            .iter()
            .map(|(k, v)| {
                serde_json::json!({
                    "key": k,
                    "value": v,
                    "type": "shared"
                })
            })
            .collect();
        serde_json::to_vec(&arr).unwrap()
    }

    #[test]
    fn env_slug_from_map() {
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = make_remote(&runner);
        assert_eq!(remote.env_slug("dev"), "development");
        assert_eq!(remote.env_slug("prod"), "production");
    }

    #[test]
    fn env_slug_fallback() {
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = make_remote(&runner);
        assert_eq!(remote.env_slug("staging"), "staging");
    }

    #[test]
    fn preflight_success() {
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"infisical/0.28.1".to_vec(),
            stderr: Vec::new(),
        }]);
        let remote = make_remote(&runner);
        assert!(remote.preflight().is_ok());
        let c = runner.calls();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].program, "infisical");
        assert_eq!(c[0].args, vec!["--version"]);
    }

    #[test]
    fn preflight_missing_cli() {
        let runner = ErrorCommandRunner::missing_command();
        let remote = make_remote(&runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("Install from:"));
    }

    #[test]
    fn push_sets_via_tempfile() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            // export (for orphan detection)
            CommandOutput {
                success: true,
                stdout: export_json(&[("API_KEY", "old"), ("_esk_version", "2")]),
                stderr: Vec::new(),
            },
            // secrets set
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]);
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test"), ("DB_URL:dev", "pg://")], 3);
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert_eq!(c.len(), 2);

        // First call: export for orphan detection
        assert_eq!(c[0].program, "infisical");
        assert!(c[0].args.contains(&"export".to_string()));
        assert!(c[0].args.contains(&"--projectId".to_string()));
        assert!(c[0].args.contains(&"proj123".to_string()));
        assert!(c[0].args.contains(&"--env".to_string()));
        assert!(c[0].args.contains(&"development".to_string()));

        // Second call: secrets set with --file
        assert_eq!(c[1].program, "infisical");
        assert!(c[1].args.contains(&"secrets".to_string()));
        assert!(c[1].args.contains(&"set".to_string()));
        assert!(c[1].args.iter().any(|a| a.starts_with("--file=")));
        assert!(c[1].args.contains(&"--silent".to_string()));
        assert!(c[1].args.contains(&"development".to_string()));
    }

    #[test]
    fn push_deletes_orphaned_keys() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            // export: remote has 3 keys, we're pushing 2
            CommandOutput {
                success: true,
                stdout: export_json(&[
                    ("API_KEY", "old"),
                    ("DB_URL", "old_pg"),
                    ("OLD_KEY", "stale"),
                    ("_esk_version", "2"),
                ]),
                stderr: Vec::new(),
            },
            // delete orphaned
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
            // secrets set
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]);
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "new_key"), ("DB_URL:dev", "new_pg")], 3);
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert_eq!(c.len(), 3);

        // Second call: delete orphaned key
        assert_eq!(c[1].program, "infisical");
        assert!(c[1].args.contains(&"secrets".to_string()));
        assert!(c[1].args.contains(&"delete".to_string()));
        assert!(c[1].args.contains(&"OLD_KEY".to_string()));
    }

    #[test]
    fn push_skips_empty_env() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = make_remote(&runner);
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert!(c.is_empty());
    }

    #[test]
    fn push_skips_delete_on_export_failure() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            // export fails (non-zero exit — e.g. auth failure)
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"project not found".to_vec(),
            },
            // secrets set still runs
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ]);
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);
        test_warnings::take();
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert_eq!(c.len(), 2);
        // No delete call — just export + set
        assert!(c[0].args.contains(&"export".to_string()));
        assert!(c[1].args.contains(&"set".to_string()));

        // The skip is silent no longer: the failure must be surfaced.
        let warnings = test_warnings::take();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("project not found"));
    }

    #[test]
    fn push_warns_and_still_sets_on_export_spawn_failure() {
        let fixture = make_config();
        let runner = MockCommandRunner::new().strict();
        // First call (export) errors at the runner level (e.g. CLI missing/spawn failure).
        // MockCommandRunner has no built-in way to make only the first call error while
        // later calls succeed, so we use ErrorCommandRunner instead — but ErrorCommandRunner
        // fails *every* call, which would also fail `secrets set`. To express "first call
        // errors, second succeeds" we fall back to `FirstCallErrorsRunner` for this test.
        runner.push_success(b"", b"");
        let wrapped = FirstCallErrorsRunner {
            inner: runner,
            calls: std::sync::Mutex::new(0),
        };
        let remote = make_remote(&wrapped);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);
        test_warnings::take();
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = wrapped.inner.calls();
        // Only `secrets set` was recorded by the inner mock (the export call went
        // through the error branch and was never delegated).
        assert_eq!(c.len(), 1);
        assert!(c[0].args.contains(&"set".to_string()));
        assert!(!c[0].args.contains(&"delete".to_string()));

        let warnings = test_warnings::take();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("No such file or directory"));
    }

    #[test]
    fn push_warns_and_still_sets_on_unparseable_export() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            // export succeeds but returns garbage, not JSON
            CommandOutput {
                success: true,
                stdout: b"not json at all".to_vec(),
                stderr: Vec::new(),
            },
            // secrets set still runs
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ])
        .strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);
        test_warnings::take();
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert_eq!(c.len(), 2);
        assert!(c[0].args.contains(&"export".to_string()));
        assert!(c[1].args.contains(&"set".to_string()));
        assert!(!c[1].args.contains(&"delete".to_string()));

        let warnings = test_warnings::take();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("could not parse"));
    }

    #[test]
    fn push_healthy_path_with_zero_orphans_stays_silent() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            // export succeeds, remote has exactly the keys we're pushing (no orphans)
            CommandOutput {
                success: true,
                stdout: export_json(&[("API_KEY", "old"), ("_esk_version", "2")]),
                stderr: Vec::new(),
            },
            // secrets set
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ])
        .strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "sk_test")], 3);
        test_warnings::take();
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert_eq!(c.len(), 2);
        assert!(!c
            .iter()
            .any(|call| call.args.contains(&"delete".to_string())));

        // Healthy path: export succeeded, zero orphans — nothing should be surfaced.
        let warnings = test_warnings::take();
        assert!(
            warnings.is_empty(),
            "expected no warnings on healthy path, got: {warnings:?}"
        );
    }

    #[test]
    fn pull_success() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: export_json(&[
                ("API_KEY", "sk_test"),
                ("DB_URL", "pg://localhost"),
                ("_esk_version", "7"),
            ]),
            stderr: Vec::new(),
        }]);
        let remote = make_remote(&runner);
        let snapshot = remote.pull(fixture.config(), "dev").unwrap().unwrap();
        let secrets = snapshot.secrets;
        let version = snapshot.version;

        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "pg://localhost");
        assert!(!secrets.contains_key("_esk_version:dev"));

        let c = runner.calls();
        assert_eq!(c.len(), 1);
        assert!(c[0].args.contains(&"export".to_string()));
        assert!(c[0].args.contains(&"development".to_string()));
    }

    #[test]
    fn pull_not_found_failure_propagates() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"project not found".to_vec(),
        }]);
        let remote = make_remote(&runner);
        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        assert_eq!(err.to_string(), "infisical export failed");
    }

    #[test]
    fn pull_auth_failure_propagates_without_stderr() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"unauthorized: token secret-token".to_vec(),
        }]);
        let remote = make_remote(&runner);
        let err = remote.pull(fixture.config(), "dev").unwrap_err();
        assert_eq!(err.to_string(), "infisical export failed");
        assert!(!err.to_string().contains("secret-token"));
    }

    #[test]
    fn pull_misleading_not_found_text_still_propagates() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"authorization path not found".to_vec(),
        }]);
        let remote = make_remote(&runner);
        assert!(remote.pull(fixture.config(), "dev").is_err());
    }

    #[test]
    fn pull_successful_empty_is_an_authoritative_snapshot() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"[]".to_vec(),
            stderr: Vec::new(),
        }]);
        let remote = make_remote(&runner);
        let snapshot = remote.pull(fixture.config(), "dev").unwrap().unwrap();
        let secrets = snapshot.secrets;
        let version = snapshot.version;
        assert!(secrets.is_empty());
        assert_eq!(version, 0);
    }

    #[test]
    fn parse_export_json_extracts_key_value() {
        let json = serde_json::to_vec(&serde_json::json!([
            {"key": "A", "value": "1", "type": "shared"},
            {"key": "B", "value": "2", "type": "personal"},
            {"key": "C", "value": "", "type": "shared"}
        ]))
        .unwrap();
        let map = parse_export_json(&json).unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map["A"], "1");
        assert_eq!(map["B"], "2");
        assert_eq!(map["C"], "");
    }
}
