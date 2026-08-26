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

/// Reject a value that cannot survive the `.env` push transport.
///
/// A carriage return is rejected alongside a newline: the CLI reads a bare CR
/// as a line terminator on some platforms, so allowing it would leave the same
/// injection open on exactly the systems least likely to be tested. The error
/// names the key but never the value, matching the redaction the other push
/// failures in this module apply.
fn reject_multiline_value(key: &str, value: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!(
            "infisical: secret '{key}' contains a newline, which the .env push format cannot \
             represent; remove the newline or use a remote that preserves multiline values"
        );
    }
    Ok(())
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

        // The transport is a `.env` file consumed by `infisical secrets set
        // --file`, so a value carrying CR/LF would continue onto a line the
        // CLI reads as a further assignment — inventing a remote key, or
        // overwriting the `_esk_version` metadata that reconciliation trusts.
        // Refuse before contacting the remote: esk does not own that parser,
        // and quoting the value would gamble a silently wrong secret against a
        // refused push.
        for (key, value) in &push_map {
            reject_multiline_value(key, value)?;
        }

        let slug = self.env_slug(env);
        let base = self.base_args(&slug);

        // Build the push map: bare keys + version metadata
        // Delete orphaned keys: export current remote state, diff, delete absent keys.
        // Export and parse are hard preconditions for every mutation. Infisical
        // set is upsert-only, so a blind partial write could retain stale keys or
        // tombstone metadata while advancing the snapshot version.
        let mut export_args = vec!["export", "--format", "json"];
        export_args.extend(base.iter().map(String::as_str));

        let export_output = self
            .runner
            .run("infisical", &export_args, CommandOpts::default())
            .context("failed to run infisical export for orphan detection")?;
        if !export_output.success {
            anyhow::bail!("infisical export failed; remote was not modified");
        }
        let remote_keys = parse_export_json(&export_output.stdout)
            .context("failed to inspect Infisical secrets for orphan deletion")?;
        let orphans: Vec<&str> = remote_keys
            .keys()
            .filter(|k| !push_map.contains_key(k.as_str()))
            .map(String::as_str)
            .collect();

        // Complete every fallible local preparation step before the first
        // remote mutation. The tempfile must stay alive until `secrets set`
        // consumes it below.
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

        if !orphans.is_empty() {
            let mut delete_args = vec!["secrets", "delete"];
            delete_args.extend(orphans);
            delete_args.extend(base.iter().map(String::as_str));

            let del_output = self
                .runner
                .run("infisical", &delete_args, CommandOpts::default())
                .context("failed to run infisical secrets delete")?;
            if !del_output.success {
                anyhow::bail!("infisical secrets delete failed");
            }
        }

        let output = self
            .runner
            .run("infisical", &set_args, CommandOpts::default())
            .context("failed to run infisical secrets set")?;
        if !output.success {
            anyhow::bail!("infisical secrets set failed");
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

    /// Strict runner that records attempted calls and fails at process spawn.
    struct SpawnErrorRunner {
        calls: std::sync::Mutex<Vec<Vec<String>>>,
    }
    impl CommandRunner for SpawnErrorRunner {
        fn run(&self, _program: &str, args: &[&str], _opts: CommandOpts) -> Result<CommandOutput> {
            self.calls
                .lock()
                .expect("SpawnErrorRunner calls mutex poisoned")
                .push(args.iter().map(|arg| (*arg).to_string()).collect());
            Err(anyhow::anyhow!("No such file or directory"))
        }
    }

    /// Strict stateful Infisical model used to exercise a failed push followed
    /// by a successful repair. `secrets set` is deliberately modeled
    /// as upsert-only, matching the provider behavior that makes this edge case
    /// correctness-sensitive.
    struct StatefulInfisicalRunner {
        remote: std::sync::Mutex<BTreeMap<String, String>>,
        export_failures_remaining: std::sync::Mutex<usize>,
        calls: std::sync::Mutex<Vec<Vec<String>>>,
        set_maps: std::sync::Mutex<Vec<BTreeMap<String, String>>>,
    }

    impl StatefulInfisicalRunner {
        fn new(remote: BTreeMap<String, String>, export_failures: usize) -> Self {
            Self {
                remote: std::sync::Mutex::new(remote),
                export_failures_remaining: std::sync::Mutex::new(export_failures),
                calls: std::sync::Mutex::new(Vec::new()),
                set_maps: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn remote(&self) -> BTreeMap<String, String> {
            self.remote
                .lock()
                .expect("StatefulInfisicalRunner remote mutex poisoned")
                .clone()
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls
                .lock()
                .expect("StatefulInfisicalRunner calls mutex poisoned")
                .clone()
        }

        fn set_maps(&self) -> Vec<BTreeMap<String, String>> {
            self.set_maps
                .lock()
                .expect("StatefulInfisicalRunner set maps mutex poisoned")
                .clone()
        }
    }

    impl CommandRunner for StatefulInfisicalRunner {
        fn run(&self, _program: &str, args: &[&str], _opts: CommandOpts) -> Result<CommandOutput> {
            self.calls
                .lock()
                .expect("StatefulInfisicalRunner calls mutex poisoned")
                .push(args.iter().map(|arg| (*arg).to_string()).collect());

            if args.starts_with(&["export", "--format", "json"]) {
                let should_fail = {
                    let mut failures = self
                        .export_failures_remaining
                        .lock()
                        .expect("StatefulInfisicalRunner export mutex poisoned");
                    let should_fail = *failures > 0;
                    *failures = failures.saturating_sub(1);
                    should_fail
                };
                if should_fail {
                    return Ok(CommandOutput {
                        success: false,
                        stdout: Vec::new(),
                        stderr: b"transient export failure".to_vec(),
                    });
                }

                let remote = self
                    .remote
                    .lock()
                    .expect("StatefulInfisicalRunner remote mutex poisoned");
                let entries: Vec<serde_json::Value> = remote
                    .iter()
                    .map(|(key, value)| {
                        serde_json::json!({
                            "key": key,
                            "value": value,
                            "type": "shared"
                        })
                    })
                    .collect();
                return Ok(CommandOutput {
                    success: true,
                    stdout: serde_json::to_vec(&entries)?,
                    stderr: Vec::new(),
                });
            }

            if args.starts_with(&["secrets", "delete"]) {
                let base_start = args
                    .iter()
                    .position(|arg| *arg == "--projectId")
                    .context("stateful test delete call missing --projectId")?;
                let mut remote = self
                    .remote
                    .lock()
                    .expect("StatefulInfisicalRunner remote mutex poisoned");
                for key in &args[2..base_start] {
                    remote.remove(*key);
                }
                return Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }

            if args.starts_with(&["secrets", "set"]) {
                let file_arg = args
                    .iter()
                    .find_map(|arg| arg.strip_prefix("--file="))
                    .context("stateful test set call missing --file")?;
                let contents = std::fs::read_to_string(file_arg)
                    .context("stateful test could not read Infisical set file")?;
                let mut set_map = BTreeMap::new();
                for line in contents.lines() {
                    let (key, value) = line
                        .split_once('=')
                        .context("stateful test found malformed Infisical set file")?;
                    set_map.insert(key.to_string(), value.to_string());
                }
                self.remote
                    .lock()
                    .expect("StatefulInfisicalRunner remote mutex poisoned")
                    .extend(set_map.clone());
                self.set_maps
                    .lock()
                    .expect("StatefulInfisicalRunner set maps mutex poisoned")
                    .push(set_map);
                return Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                });
            }

            anyhow::bail!("unexpected Infisical command in stateful test")
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
    fn push_rejects_newline_value_before_contacting_remote() {
        let fixture = make_config();
        // Strict with no queued outputs: any CLI call at all fails the test.
        let runner = MockCommandRunner::from_outputs(Vec::new()).strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "line1\nDB_URL=injected")], 3);

        let err = remote.push(&payload, fixture.config(), "dev").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("API_KEY"), "error should name the key: {msg}");
        assert!(msg.contains("newline"), "error should explain why: {msg}");
        assert!(
            !msg.contains("injected"),
            "error must not echo the secret value: {msg}"
        );
        assert!(
            runner.calls().is_empty(),
            "no infisical call may be made when a value is rejected"
        );
    }

    #[test]
    fn push_rejects_carriage_return_value() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(Vec::new()).strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "line1\r_esk_version=999")], 3);

        let err = remote.push(&payload, fixture.config(), "dev").unwrap_err();
        assert!(err.to_string().contains("API_KEY"));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn push_accepts_values_without_line_breaks() {
        // Guards the rejection from over-reaching: spaces, quotes, `#`, and
        // `=` inside a value are all legal and must still push.
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: export_json(&[]),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            },
        ])
        .strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "a b \"c\" #d =e")], 3);

        remote.push(&payload, fixture.config(), "dev").unwrap();
        assert_eq!(runner.calls().len(), 2);
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
        ])
        .strict();
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
        ])
        .strict();
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
        let runner = MockCommandRunner::from_outputs(vec![]).strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("KEY:prod", "val")], 1);
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert!(c.is_empty());
    }

    #[test]
    fn push_fails_closed_on_export_nonzero() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: b"unauthorized: token secret-token".to_vec(),
        }])
        .strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);
        let err = remote.push(&payload, fixture.config(), "dev").unwrap_err();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].args.contains(&"export".to_string()));
        assert_eq!(
            err.to_string(),
            "infisical export failed; remote was not modified"
        );
        assert!(!err.to_string().contains("secret-token"));
    }

    #[test]
    fn push_fails_closed_on_export_spawn_failure() {
        let fixture = make_config();
        let runner = SpawnErrorRunner {
            calls: std::sync::Mutex::new(Vec::new()),
        };
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);
        let err = remote.push(&payload, fixture.config(), "dev").unwrap_err();

        let calls = runner
            .calls
            .lock()
            .expect("SpawnErrorRunner calls mutex poisoned");
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&"export".to_string()));
        assert!(err
            .to_string()
            .contains("failed to run infisical export for orphan detection"));
        assert!(format!("{err:#}").contains("No such file or directory"));
    }

    #[test]
    fn push_fails_closed_on_unparseable_export() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: b"not json at all".to_vec(),
            stderr: Vec::new(),
        }])
        .strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);
        let err = remote.push(&payload, fixture.config(), "dev").unwrap_err();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].args.contains(&"export".to_string()));
        assert!(err
            .to_string()
            .contains("failed to inspect Infisical secrets for orphan deletion"));
    }

    #[test]
    fn push_set_failure_is_primary_and_redacted() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: export_json(&[("API_KEY", "old"), ("_esk_version", "1")]),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"set-secret-marker".to_vec(),
            },
        ])
        .strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);

        let err = remote.push(&payload, fixture.config(), "dev").unwrap_err();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].args.contains(&"export".to_string()));
        assert!(calls[1].args.contains(&"set".to_string()));
        assert_eq!(err.to_string(), "infisical secrets set failed");
        assert!(!err.to_string().contains("set-secret-marker"));
    }

    #[test]
    fn failed_push_preserves_consistent_remote_and_successful_retry_repairs_it() {
        let fixture = make_config();
        let old_tombstones =
            serde_json::to_string(&BTreeMap::from([("RESURRECT".to_string(), 1_u64)])).unwrap();
        let initial_remote = BTreeMap::from([
            ("DELETE_ME".to_string(), "old-delete".to_string()),
            ("KEEP".to_string(), "old-live".to_string()),
            (crate::remotes::ESK_VERSION_KEY.to_string(), "1".to_string()),
            (
                crate::remotes::ESK_TOMBSTONES_KEY.to_string(),
                old_tombstones,
            ),
        ]);
        let runner = StatefulInfisicalRunner::new(initial_remote.clone(), 1);
        let remote = make_remote(&runner);
        let payload = StorePayload {
            secrets: BTreeMap::from([
                ("KEEP:dev".to_string(), "new-live".to_string()),
                ("RESURRECT:dev".to_string(), "restored".to_string()),
            ]),
            version: 2,
            tombstones: BTreeMap::from([("DELETE_ME:dev".to_string(), 2)]),
            env_versions: BTreeMap::from([("dev".to_string(), 2)]),
            ..Default::default()
        };

        let first_error = remote.push(&payload, fixture.config(), "dev").unwrap_err();
        assert!(first_error.to_string().contains("infisical export failed"));
        assert_eq!(runner.calls().len(), 1);
        assert!(runner.set_maps().is_empty());
        assert_eq!(runner.remote(), initial_remote);

        // No artificial version advancement: the old, internally consistent
        // snapshot remains parseable and lower than the local checkout.
        let unchanged_snapshot =
            crate::remotes::parse_flat_snapshot(runner.remote(), "dev").unwrap();
        assert_eq!(unchanged_snapshot.version, 1);
        assert_eq!(
            unchanged_snapshot
                .secrets
                .get("DELETE_ME:dev")
                .map(String::as_str),
            Some("old-delete")
        );
        assert_eq!(unchanged_snapshot.tombstones.get("RESURRECT:dev"), Some(&1));

        let behind_checkout = StorePayload::default();
        for preference in [
            crate::reconcile::ConflictPreference::Local,
            crate::reconcile::ConflictPreference::Remote,
        ] {
            let reconciliation = crate::reconcile::reconcile_multi_snapshots_with_jump_limit(
                &behind_checkout,
                &[("infisical", &unchanged_snapshot)],
                "dev",
                preference,
                true,
            )
            .unwrap();
            assert_eq!(reconciliation.merged_payload.env_version("dev"), 1);
            assert_eq!(
                reconciliation
                    .merged_payload
                    .secrets
                    .get("DELETE_ME:dev")
                    .map(String::as_str),
                Some("old-delete")
            );
            assert_eq!(
                reconciliation
                    .merged_payload
                    .tombstones
                    .get("RESURRECT:dev"),
                Some(&1)
            );
        }

        for preference in [
            crate::reconcile::ConflictPreference::Local,
            crate::reconcile::ConflictPreference::Remote,
        ] {
            let reconciliation = crate::reconcile::reconcile_multi_snapshots_with_jump_limit(
                &payload,
                &[("infisical", &unchanged_snapshot)],
                "dev",
                preference,
                true,
            )
            .unwrap();
            assert_eq!(reconciliation.merged_payload.env_version("dev"), 2);
            assert_eq!(reconciliation.sources_to_update, vec!["infisical"]);
        }

        // The failed acknowledgement blocks deletion GC before the retry.
        let tracker_dir = tempfile::tempdir().unwrap();
        let mut tracker =
            crate::sync_tracker::SyncIndex::new(&tracker_dir.path().join("sync-index.json"));
        tracker.record_failure("infisical", "dev", 2, first_error.to_string());
        let mut failed_gc = payload.clone();
        assert_eq!(failed_gc.prune_tombstones(&tracker, &["infisical"]), 0);
        assert!(failed_gc.tombstones.contains_key("DELETE_ME:dev"));

        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 4);
        assert!(calls[0].starts_with(&["export".to_string()]));
        assert!(calls[1].starts_with(&["export".to_string()]));
        assert!(calls[2].starts_with(&["secrets".to_string(), "delete".to_string()]));
        assert!(calls[3].starts_with(&["secrets".to_string(), "set".to_string()]));
        assert!(calls[2].contains(&"DELETE_ME".to_string()));

        let set_maps = runner.set_maps();
        assert_eq!(set_maps.len(), 1);
        assert_eq!(
            set_maps[0].get("RESURRECT").map(String::as_str),
            Some("restored")
        );
        assert_eq!(
            set_maps[0]
                .get(crate::remotes::ESK_VERSION_KEY)
                .map(String::as_str),
            Some("2")
        );
        assert!(set_maps[0].contains_key(crate::remotes::ESK_TOMBSTONES_KEY));
        let repaired_snapshot =
            crate::remotes::parse_flat_snapshot(runner.remote(), "dev").unwrap();
        assert_eq!(repaired_snapshot.version, 2);
        assert_eq!(
            repaired_snapshot
                .secrets
                .get("RESURRECT:dev")
                .map(String::as_str),
            Some("restored")
        );
        assert!(!repaired_snapshot.secrets.contains_key("DELETE_ME:dev"));
        assert!(!repaired_snapshot.tombstones.contains_key("RESURRECT:dev"));
        assert_eq!(repaired_snapshot.tombstones.get("DELETE_ME:dev"), Some(&2));

        tracker.record_success("infisical", "dev", 2);
        let mut successful_gc = payload;
        assert_eq!(successful_gc.prune_tombstones(&tracker, &["infisical"]), 1);
        assert!(successful_gc.tombstones.is_empty());
    }

    #[test]
    fn push_delete_failure_is_primary_and_stops_before_set() {
        let fixture = make_config();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: export_json(&[("API_KEY", "old"), ("OLD_KEY", "stale")]),
                stderr: Vec::new(),
            },
            CommandOutput {
                success: false,
                stdout: Vec::new(),
                stderr: b"delete-secret-marker".to_vec(),
            },
        ])
        .strict();
        let remote = make_remote(&runner);
        let payload = make_payload(&[("API_KEY:dev", "val")], 1);

        let err = remote.push(&payload, fixture.config(), "dev").unwrap_err();

        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].args.contains(&"export".to_string()));
        assert!(calls[1].args.contains(&"delete".to_string()));
        assert!(!calls[1].args.contains(&"set".to_string()));
        assert_eq!(err.to_string(), "infisical secrets delete failed");
        assert!(!err.to_string().contains("delete-secret-marker"));
    }

    #[test]
    fn push_healthy_path_with_zero_orphans_sets_after_export() {
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
        remote.push(&payload, fixture.config(), "dev").unwrap();

        let c = runner.calls();
        assert_eq!(c.len(), 2);
        assert!(!c
            .iter()
            .any(|call| call.args.contains(&"delete".to_string())));
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
