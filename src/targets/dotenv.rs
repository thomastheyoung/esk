//! .env file target — writes secrets to a local `.env` file.
//!
//! Not a cloud service — generates standard dotenv files consumed by most
//! frameworks and runtimes (Node.js, Python, Ruby, etc.).
//!
//! Operates in **batch mode**: the entire file is regenerated atomically on
//! every deploy via temp-file-then-rename. Deletions are handled implicitly
//! by omitting the key from the next write. Values containing newlines are
//! rejected by `validate_dotenv_value` before formatting.

use anyhow::{Context, Result};
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use crate::config::{Config, OutputPathStatus, ResolvedTarget};
use crate::targets::{
    BatchDeployment, DeployMode, DeployOutcome, DeployResult, DeployTarget, SecretValue,
};
use crate::verify::{Evidence, Fidelity};

/// Format a value for safe inclusion in a .env file.
///
/// If the value contains characters that could cause parsing issues (double
/// quotes, backslashes, spaces, `#`, or starts with `=`), wraps it in double
/// quotes with proper escaping. Newlines are rejected earlier by
/// `validate_dotenv_value` and never reach this function.
fn format_env_value(value: &str) -> String {
    let needs_quoting = value.contains('"')
        || value.contains('\\')
        || value.contains(' ')
        || value.contains('#')
        || value.starts_with('=');

    if !needs_quoting {
        return value.to_string();
    }

    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn validate_dotenv_value(key: &str, value: &str) -> Result<()> {
    if value.contains('\n') || value.contains('\r') {
        anyhow::bail!(
            ".env: secret '{key}' contains newlines, refusing to write multiline values to .env files"
        );
    }
    Ok(())
}

pub struct DotenvTarget<'a> {
    pub config: &'a Config,
}

impl DeployTarget for DotenvTarget<'_> {
    fn name(&self) -> &'static str {
        ".env"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Batch
    }

    fn deploy_secret(&self, _key: &str, _value: &str, _target: &ResolvedTarget) -> Result<()> {
        // Env target always uses deploy_batch — individual deploy is a no-op
        // because we need to regenerate the entire file atomically
        Ok(())
    }

    /// Override: regenerate the entire env file for this (app, env) pair.
    /// Compare the file on disk against exactly what a deploy would write.
    ///
    /// The generated file is byte-stable for a given secret set, so equality
    /// against `render_dotenv_content` is a complete check: it catches a
    /// deleted file, an edited value, and a hand-added key alike. `None` is
    /// returned when the answer would be a guess rather than a fact — no app
    /// on the target, an unresolvable path, or an unreadable file.
    fn artifact_matches(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Option<bool> {
        let app = target.app.as_ref()?;
        let expected = render_dotenv_content(secrets).ok()?;
        // Path resolution enforces esk's output policy, and its refusals mean
        // two different things here.
        //
        // A symlink is a configuration the user chose. esk has never written
        // through one, so the group was previously skipped and stayed quiet;
        // calling it drift would mark it dirty every run, and the deploy would
        // then fail on the same policy with no way for the user to clear it.
        // Report "cannot tell" and leave the decision to the store.
        //
        // Anything else — most importantly a directory sitting where the file
        // belongs — is a state esk did not create, so it stays a mismatch and
        // fails loudly rather than being reported as current.
        let Ok(path) = self.config.resolve_dotenv_path(app, &target.environment) else {
            return match self.config.classify_dotenv_path(app, &target.environment) {
                // A symlink anywhere in the path is the user's layout.
                OutputPathStatus::TraversesSymlink => None,
                _ => Some(false),
            };
        };
        // Compare bytes, not text. A hand-mangled file may not be valid UTF-8,
        // and `render_dotenv_content` always is, so decoding first would turn a
        // definite mismatch into "cannot tell" and leave the file corrupt.
        match std::fs::read(&path) {
            Ok(actual) => Some(actual == expected.as_bytes()),
            // Anything esk cannot read back from a path it exclusively owns is
            // treated as a mismatch, not an unknown. A missing file, a
            // directory where the file belongs, or a file esk wrote as
            // owner-readable and can no longer open are all states esk did not
            // create.
            //
            // This is deliberately blunt about transient errors too, because
            // the two ways of being wrong are not symmetric: healing
            // needlessly costs one write of byte-identical content, since the
            // store is the source of truth and rendering is deterministic,
            // whereas reporting "cannot tell" leaves a broken artifact behind
            // a successful-looking run — the failure this check exists to
            // prevent.
            Err(_) => Some(false),
        }
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read the generated file back and return the values it holds.
    ///
    /// The cheapest read-back esk has — a local file, no network — and the one
    /// where out-of-band edits are most likely, since the artifact sits in the
    /// developer's working tree.
    ///
    /// Follows symlinks, unlike [`DeployTarget::artifact_matches`]: that
    /// method answers a question about *writing*, which esk refuses to do
    /// through a link. Reading through one cannot damage it, and declining to
    /// look would leave an artifact esk never inspected reported as current.
    ///
    /// A missing file is reported as an empty read, so every managed key comes
    /// back `Missing` — the truth, since the file is not there. A file that
    /// exists but cannot be read is an error instead: esk did not observe
    /// those keys and must not claim they are absent.
    ///
    /// Following a symlink can therefore surface key *names* from outside the
    /// project in the `extra` list. Values never leave this function, and the
    /// link is the user's own configuration.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let app = target.app.as_ref().context(".env target requires an app")?;
        let path = self
            .config
            .dotenv_display_path(app, &target.environment)
            .with_context(|| format!("no .env path configured for '{app}'"))?;

        let contents = match std::fs::read(&path) {
            Ok(contents) => contents,
            // A deleted artifact is the state this check exists to catch, and
            // esk did observe its absence: the file holds none of the managed
            // keys, so reporting them missing is a fact.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Evidence::Values(BTreeMap::new()))
            }
            // Present but unreadable is a different state, and collapsing it
            // into the one above fabricates a verdict: esk never saw these
            // keys, so it cannot report them absent. `Missing` would send the
            // operator to redeploy, which fails on the same permissions and
            // explains nothing. The `io` error names the real problem.
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };

        // Lossy decode rather than an error: a hand-mangled file may not be
        // valid UTF-8, and refusing to read it would report a corrupt artifact
        // as unverifiable. Any mangled key still fails to match its stored
        // value, which is the outcome the operator needs to see.
        let text = String::from_utf8_lossy(&contents);
        Ok(Evidence::Values(
            text.lines()
                .filter_map(parse_env_line)
                .map(|(key, value)| (key, Zeroizing::new(value)))
                .collect(),
        ))
    }

    fn deploy_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<DeployResult> {
        self.deploy_batch_state(BatchDeployment::without_tombstones(secrets), target)
            .unwrap_or_else(|error| {
                secrets
                    .iter()
                    .map(|secret| DeployResult {
                        key: secret.key.clone(),
                        outcome: DeployOutcome::Failed(error.to_string()),
                    })
                    .collect()
            })
    }

    fn deploy_batch_state(
        &self,
        batch: BatchDeployment<'_>,
        target: &ResolvedTarget,
    ) -> Result<Vec<DeployResult>> {
        let Some(app) = &target.app else {
            anyhow::bail!(".env target requires an app");
        };

        self.write_dotenv_file(app, &target.environment, batch.secrets)?;
        Ok(batch
            .secrets
            .iter()
            .map(|s| DeployResult {
                key: s.key.clone(),
                outcome: DeployOutcome::Success,
            })
            .collect())
    }
}

/// Parse one `KEY=VALUE` line back into its key and unescaped value.
///
/// The exact inverse of [`format_env_value`]: a quoted value is unwrapped and
/// its `\\` and `\"` escapes undone. Reading with a looser grammar than the
/// one that wrote the file would report false drift on every value containing
/// a space or `#`, since those are written quoted.
///
/// Returns `None` for comments, blank lines, and anything without a separator.
fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim_end_matches('\r');
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    // Split on the first `=`; values legitimately contain more.
    let (key, raw) = line.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }

    let value = match raw.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        // Scan once rather than chaining `replace` calls. For text this
        // module wrote the two agree, since `format_env_value` escapes every
        // backslash before every quote and the pair-wise replace can never
        // leave a stray one behind. Scanning is kept because it does not
        // depend on that coupling: it stays correct for a hand-edited file
        // and for any future change to the writer's escaping.
        Some(inner) => {
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some(next) => out.push(next),
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            out
        }
        None => raw.to_string(),
    };
    Some((key.to_string(), value))
}

/// Render the exact file contents `deploy_batch` would write for `secrets`.
///
/// Kept separate from the write so verification can compare against it. Both
/// callers share this one function, so a check can never drift from the format
/// it is checking — which a hand-written parser of the same grammar would.
///
/// Output is deterministic: groups come from a `BTreeMap`, keys are sorted
/// within a group, the header is fixed, and no timestamp is embedded.
pub(crate) fn render_dotenv_content(secrets: &[SecretValue]) -> Result<String> {
    // Group secrets by group, maintaining sorted order
    let mut by_group: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for secret in secrets {
        validate_dotenv_value(&secret.key, &secret.value)?;
        by_group
            .entry(&secret.group)
            .or_default()
            .push((&secret.key, secret.value.as_str()));
    }

    let mut content = String::new();
    content.push_str("# Auto-generated by esk — do not edit manually\n");
    content.push_str("#\n");
    content.push_str("# Update secrets:  esk set <KEY> --env <ENV>\n");
    content.push_str("# Regenerate file: esk deploy --env <ENV>\n");

    for (group, mut entries) in by_group {
        entries.sort_by_key(|(k, _)| *k);
        content.push('\n');
        let _ = writeln!(content, "# === {group} ===");
        for (key, value) in entries {
            let _ = writeln!(content, "{key}={}", format_env_value(value));
        }
    }

    Ok(content)
}

impl DotenvTarget<'_> {
    /// Compare the artifact on disk against what a deploy would write, reading
    /// through any symlink in the path.
    ///
    /// [`DeployTarget::artifact_matches`] answers a question about *writing*,
    /// so it declines on a symlinked path esk must never write through. A
    /// reader is under no such restriction: following the link to check what is
    /// there cannot damage it, and refusing to look would report an artifact
    /// esk never inspected as current.
    pub(crate) fn artifact_matches_readonly(
        &self,
        secrets: &[SecretValue],
        target: &ResolvedTarget,
    ) -> Option<bool> {
        let app = target.app.as_ref()?;
        let expected = render_dotenv_content(secrets).ok()?;
        let path = self.config.dotenv_display_path(app, &target.environment)?;
        match std::fs::read(&path) {
            Ok(actual) => Some(actual == expected.as_bytes()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(false),
            // Present but unreadable: still a state esk did not create.
            Err(_) => Some(false),
        }
    }

    fn write_dotenv_file(&self, app: &str, env: &str, secrets: &[SecretValue]) -> Result<()> {
        self.write_dotenv_file_inner(app, env, secrets, |_| {})
    }

    /// The hook keeps the parent-creation/revalidation boundary testable; the
    /// production caller supplies a no-op.
    fn write_dotenv_file_inner<F>(
        &self,
        app: &str,
        env: &str,
        secrets: &[SecretValue],
        after_parent_creation: F,
    ) -> Result<()>
    where
        F: FnOnce(&std::path::Path),
    {
        let path = self.config.resolve_dotenv_path(app, env)?;
        let content = render_dotenv_content(secrets)?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        after_parent_creation(&path);

        // Parent creation can race with a path replacement. Resolve again
        // immediately before creating or promoting the temporary file.
        let path = self.config.resolve_dotenv_path(app, env)?;

        // Atomic write
        let dir = path.parent().context("env path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), &content)?;
        tmp.persist(&path)
            .with_context(|| format!("failed to write {}", path.display()))?;

        // Mark read-only to discourage manual edits
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))?;
        }
        #[cfg(not(unix))]
        {
            let mut perms = std::fs::metadata(&path)?.permissions();
            perms.set_readonly(true);
            std::fs::set_permissions(&path, perms)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::targets::SecretValue;
    use crate::test_support::ConfigFixture;
    use std::os::unix::fs::PermissionsExt;

    const DOTENV_YAML: &str = r#"
project: testapp
environments: [dev, prod]
apps:
  web:
    path: apps/web
targets:
  .env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
      prod: ".production"
"#;

    fn make_fixture() -> ConfigFixture {
        ConfigFixture::new(DOTENV_YAML).unwrap()
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: ".env".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    fn make_secret(key: &str, value: &str, group: &str) -> SecretValue {
        SecretValue {
            key: key.to_string(),
            value: zeroize::Zeroizing::new(value.to_string()),
            group: group.to_string(),
        }
    }

    #[test]
    fn deploy_secret_is_noop() {
        let fixture = make_fixture();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap();
    }

    #[test]
    fn deploy_batch_no_app_errors() {
        let fixture = make_fixture();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![make_secret("A", "1", "G"), make_secret("B", "2", "G")];
        let error = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(None, "dev"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("requires an app"));
    }

    #[test]
    fn deploy_batch_writes_env_file() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![make_secret("KEY", "value123", "General")];
        let results = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        assert!(results.iter().all(|r| r.outcome.is_success()));
        let content = std::fs::read_to_string(fixture.path("apps/web/.env.local")).unwrap();
        assert!(content.contains("KEY=value123"));
    }

    #[test]
    fn env_file_header() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![make_secret("K", "v", "G")];
        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let content = std::fs::read_to_string(fixture.path("apps/web/.env.local")).unwrap();
        assert!(content.starts_with("# Auto-generated by esk"));
    }

    #[test]
    fn env_file_grouped_by_group() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![
            make_secret("A", "1", "Stripe"),
            make_secret("B", "2", "Convex"),
        ];
        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let content = std::fs::read_to_string(fixture.path("apps/web/.env.local")).unwrap();
        assert!(content.contains("# === Stripe ==="));
        assert!(content.contains("# === Convex ==="));
    }

    #[test]
    fn env_file_sorted_within_group() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![
            make_secret("ZEBRA", "z", "G"),
            make_secret("APPLE", "a", "G"),
        ];
        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let content = std::fs::read_to_string(fixture.path("apps/web/.env.local")).unwrap();
        let apple_pos = content.find("APPLE=").unwrap();
        let zebra_pos = content.find("ZEBRA=").unwrap();
        assert!(apple_pos < zebra_pos);
    }

    #[test]
    fn env_file_creates_parent_dirs() {
        let fixture = make_fixture();
        // Don't pre-create apps/web — target should create it
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![make_secret("K", "v", "G")];
        let results = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        assert!(results[0].outcome.is_success());
        assert!(fixture.path("apps/web/.env.local").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn write_rejects_symlink_added_after_an_earlier_path_resolution() {
        use std::os::unix::fs::symlink;

        let fixture = make_fixture();
        let initial = fixture.config().resolve_dotenv_path("web", "dev").unwrap();
        assert_eq!(initial, fixture.path("apps/web/.env.local"));

        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join(".env.local");
        std::fs::write(&sentinel, "outside sentinel\n").unwrap();

        let target = DotenvTarget {
            config: fixture.config(),
        };
        let err = target
            .write_dotenv_file_inner(
                "web",
                "dev",
                &[make_secret("KEY", "value", "General")],
                |path| {
                    let parent = path.parent().unwrap();
                    assert!(parent.is_dir());
                    std::fs::remove_dir_all(parent).unwrap();
                    symlink(outside.path(), parent).unwrap();
                },
            )
            .unwrap_err();
        assert!(format!("{err:#}").contains("traverses a symlink"));
        assert_eq!(
            std::fs::read_to_string(&sentinel).unwrap(),
            "outside sentinel\n"
        );
    }

    #[test]
    fn deploy_batch_all_success() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![
            make_secret("A", "1", "G"),
            make_secret("B", "2", "G"),
            make_secret("C", "3", "G"),
        ];
        let results = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|r| r.outcome.is_success()));
    }

    #[test]
    fn format_env_value_plain() {
        assert_eq!(format_env_value("simple_value"), "simple_value");
    }

    #[test]
    fn format_env_value_with_double_quote() {
        assert_eq!(format_env_value(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn format_env_value_with_backslash() {
        assert_eq!(format_env_value(r"path\to"), r#""path\\to""#);
    }

    #[test]
    fn format_env_value_with_hash() {
        assert_eq!(format_env_value("val#comment"), "\"val#comment\"");
    }

    #[test]
    fn format_env_value_with_space() {
        assert_eq!(format_env_value("has space"), "\"has space\"");
    }

    #[test]
    fn format_env_value_starts_with_equals() {
        assert_eq!(format_env_value("=oops"), "\"=oops\"");
    }

    #[test]
    fn deploy_batch_newline_value_is_rejected() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![make_secret("CERT", "line1\nline2", "General")];
        let error = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap_err();
        assert!(error.to_string().contains("contains newlines"));
        assert!(!fixture.path("apps/web/.env.local").exists());
    }

    #[test]
    #[cfg(unix)]
    fn env_file_has_restricted_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![make_secret("KEY", "value", "General")];
        let results = target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        assert!(results.iter().all(|r| r.outcome.is_success()));
        let metadata = std::fs::metadata(fixture.path("apps/web/.env.local")).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o400);
    }

    #[test]
    fn deploy_batch_write_error() {
        // This test mutates config.root after loading, so it uses Config::load directly.
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
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let mut config = Config::load(&path).unwrap();
        // Point root to a read-only location to force write failure
        config.root = std::path::PathBuf::from("/nonexistent/root");
        let target = DotenvTarget { config: &config };
        let secrets = vec![make_secret("K", "v", "G")];
        assert!(target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&secrets),
                &make_target(Some("web"), "dev")
            )
            .is_err());
    }

    #[test]
    fn deploy_batch_empty_propagates_final_secret_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(
            &path,
            r#"
project: x
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  .env:
    pattern: "{app_path}/.env"
"#,
        )
        .unwrap();
        let mut config = Config::load(&path).unwrap();
        config.root = std::path::PathBuf::from("/nonexistent/root");
        let target = DotenvTarget { config: &config };

        assert!(target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[]),
                &make_target(Some("web"), "dev")
            )
            .is_err());
    }

    fn verify_keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn verify_expected(pairs: &[(&str, &str)]) -> BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    /// The property that matters most: whatever the writer writes, the reader
    /// must read back byte-identically. A reader with a looser grammar than
    /// `format_env_value` reports false drift on correct files — noise the
    /// operator learns to ignore, which is how a real drift gets missed.
    #[test]
    fn dotenv_read_back_round_trips_every_value_the_writer_quotes() {
        let awkward = [
            ("PLAIN", "simple"),
            ("WITH_SPACE", "hello world"),
            ("WITH_HASH", "value#notacomment"),
            ("WITH_QUOTE", "say \"hi\""),
            ("WITH_BACKSLASH", r"C:\path\to"),
            ("BACKSLASH_THEN_QUOTE", r"trail\"),
            ("LEADING_EQUALS", "=starts"),
            ("EMPTY", ""),
            ("EQUALS_INSIDE", "a=b=c"),
            ("URL", "postgres://u:p@h/db?x=1"),
            ("BASE64_PAD", "YWJjZA=="),
        ];
        let secrets: Vec<SecretValue> = awkward
            .iter()
            .map(|(k, v)| make_secret(k, v, "G"))
            .collect();

        let rendered = render_dotenv_content(&secrets).unwrap();
        let parsed: BTreeMap<String, String> =
            rendered.lines().filter_map(parse_env_line).collect();

        for (key, value) in awkward {
            assert_eq!(
                parsed.get(key).map(String::as_str),
                Some(value),
                "value for {key} did not survive a write/read round trip"
            );
        }
        // The header comment lines must not become phantom keys.
        assert_eq!(parsed.len(), awkward.len());
    }

    #[test]
    fn dotenv_read_back_returns_the_files_values() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target = DotenvTarget { config };
        let resolved = make_target(Some("web"), "dev");

        let secrets = vec![
            make_secret("API_KEY", "secret1", "G"),
            make_secret("DB_URL", "postgres://x", "G"),
        ];
        target
            .deploy_batch_state(BatchDeployment::without_tombstones(&secrets), &resolved)
            .unwrap();

        let evidence = target
            .read_back(&verify_keys(&["API_KEY", "DB_URL"]), &resolved)
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!(".env declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");
        assert_eq!(values["DB_URL"].as_str(), "postgres://x");
    }

    /// The negative case. A happy-path test cannot tell a working reader from
    /// one that echoes back the store.
    #[test]
    fn dotenv_read_back_surfaces_an_edited_value_as_drift() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target = DotenvTarget { config };
        let resolved = make_target(Some("web"), "dev");

        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[make_secret("API_KEY", "correct", "G")]),
                &resolved,
            )
            .unwrap();

        // Someone edits the file by hand. esk writes it mode 0o400, so an
        // in-place write fails — tampering means replacing the file, which is
        // what an editor's save does anyway.
        let path = config.dotenv_display_path("web", "dev").unwrap();
        let edited = std::fs::read_to_string(&path)
            .unwrap()
            .replace("correct", "TAMPERED");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, edited).unwrap();

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &resolved),
            &verify_expected(&[("API_KEY", "correct")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Differs);
    }

    /// A deleted artifact is the case that made this whole feature necessary:
    /// the deploy index still says the secret was sent.
    #[test]
    fn dotenv_read_back_reports_a_deleted_file_as_missing_keys() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target = DotenvTarget { config };
        let resolved = make_target(Some("web"), "dev");

        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[make_secret("API_KEY", "v", "G")]),
                &resolved,
            )
            .unwrap();
        let path = config.dotenv_display_path("web", "dev").unwrap();
        std::fs::remove_file(&path).unwrap();

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &resolved),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true },
            "a deleted artifact must read as drift, not as verified"
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Missing);
    }

    #[test]
    fn dotenv_read_back_reports_a_hand_added_key_as_extra() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target = DotenvTarget { config };
        let resolved = make_target(Some("web"), "dev");

        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[make_secret("API_KEY", "v", "G")]),
                &resolved,
            )
            .unwrap();
        let path = config.dotenv_display_path("web", "dev").unwrap();
        let mut contents = std::fs::read_to_string(&path).unwrap();
        contents.push_str("SNUCK_IN=surprise\n");
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, contents).unwrap();

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &resolved),
            &verify_expected(&[("API_KEY", "v")]),
        );
        let crate::verify::Findings::Values { extra, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(extra, &["SNUCK_IN".to_string()]);
    }

    #[test]
    fn dotenv_read_back_follows_a_symlinked_path() {
        // `artifact_matches` declines on a symlink because it answers a
        // question about writing. Reading through one is safe, and refusing
        // would report an artifact esk never looked at as current.
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        fixture.create_dir_all("elsewhere").unwrap();
        let config = fixture.config();
        let target = DotenvTarget { config };
        let resolved = make_target(Some("web"), "dev");

        let real = config.root.join("elsewhere/.env.real");
        std::fs::write(&real, "API_KEY=linked\n").unwrap();
        let link = config.dotenv_display_path("web", "dev").unwrap();
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &resolved)
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values");
        };
        assert_eq!(values["API_KEY"].as_str(), "linked");
    }

    /// Hand-edited files are not constrained by `format_env_value`'s grammar.
    /// A lone backslash before a quote — escaping the writer never emits,
    /// because it always doubles backslashes first — must still unescape the
    /// way a dotenv reader would, rather than by pair-wise substitution.
    #[test]
    fn dotenv_read_back_unescapes_hand_written_sequences() {
        // `"a\"b"` — one backslash escaping the quote, as a human would type.
        assert_eq!(
            parse_env_line(r#"K="a\"b""#),
            Some(("K".to_string(), r#"a"b"#.to_string()))
        );
        // A backslash with nothing after it inside the quotes is kept as a
        // literal, rather than swallowing the closing quote or being dropped.
        assert_eq!(
            parse_env_line(r#"K="ends\""#),
            Some(("K".to_string(), "ends\\".to_string()))
        );
        // Two backslashes escaping one quote: the writer's own form.
        assert_eq!(
            parse_env_line(r#"K="a\\b""#),
            Some(("K".to_string(), r"a\b".to_string()))
        );
        // A backslash before an ordinary character. This is the one input
        // where a pair-wise `replace` chain disagrees with scanning: it would
        // leave `\a`, keeping an escape character the reader already consumed.
        // Scanning drops the backslash, matching how dotenv readers behave.
        assert_eq!(
            parse_env_line(r#"K="\a""#),
            Some(("K".to_string(), "a".to_string()))
        );
    }

    #[test]
    fn dotenv_read_back_ignores_comments_and_blank_lines() {
        assert_eq!(parse_env_line("# a comment"), None);
        assert_eq!(parse_env_line("   # indented comment"), None);
        assert_eq!(parse_env_line(""), None);
        assert_eq!(parse_env_line("   "), None);
        assert_eq!(parse_env_line("no_separator_here"), None);
        assert_eq!(parse_env_line("=novalue"), None);
    }

    /// An indented comment that also contains `=` must stay a comment.
    ///
    /// Without this the comment check can be made to look at the untrimmed
    /// line and every test still passes, because the plain indented comment
    /// above has no `=` and returns `None` for the wrong reason.
    #[test]
    fn dotenv_read_back_ignores_an_indented_comment_containing_equals() {
        assert_eq!(parse_env_line("  # note: a=b is fine"), None);
        assert_eq!(parse_env_line("\t# tabbed=comment"), None);
    }

    /// A CRLF file must not leave `\r` glued to the end of every value.
    ///
    /// esk writes LF, but a Windows editor or a `git` checkout with
    /// `core.autocrlf` rewrites the artifact, and a trailing `\r` would make
    /// every single value mismatch — a whole scope of false drift.
    #[test]
    fn dotenv_read_back_strips_carriage_returns_from_crlf_files() {
        assert_eq!(
            parse_env_line("KEY=value\r"),
            Some(("KEY".to_string(), "value".to_string()))
        );
        // Also inside quotes, where the `\r` would land within the value.
        assert_eq!(
            parse_env_line("KEY=\"has space\"\r"),
            Some(("KEY".to_string(), "has space".to_string()))
        );
    }

    /// Whitespace around a key must not become part of the key name.
    ///
    /// A key read as `" API_KEY"` matches nothing in the store, so it is
    /// reported both as the managed key missing and as an unmanaged extra —
    /// two wrong findings from one stray space.
    #[test]
    fn dotenv_read_back_trims_whitespace_around_keys() {
        assert_eq!(
            parse_env_line("  API_KEY=v"),
            Some(("API_KEY".to_string(), "v".to_string()))
        );
        assert_eq!(
            parse_env_line("API_KEY =v"),
            Some(("API_KEY".to_string(), "v".to_string()))
        );
    }

    #[test]
    fn dotenv_read_back_requires_an_app() {
        let fixture = make_fixture();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let Err(err) = target.read_back(&verify_keys(&["A"]), &make_target(None, "dev")) else {
            panic!("a target with no app cannot resolve a path and must not report an empty read");
        };
        assert!(err.to_string().contains("requires an app"));
    }

    /// A file esk cannot read must not be reported as a file esk knows to be
    /// empty. `Missing` is a claim about a key esk never observed.
    #[test]
    fn dotenv_read_back_unreadable_file_is_unreachable_not_missing() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target = DotenvTarget { config };
        let resolved = make_target(Some("web"), "dev");

        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[make_secret("API_KEY", "v", "G")]),
                &resolved,
            )
            .unwrap();
        let path = config.dotenv_display_path("web", "dev").unwrap();
        // The file is present and holds exactly the right content.
        std::fs::set_permissions(&path, PermissionsExt::from_mode(0o000)).unwrap();

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &resolved),
            &verify_expected(&[("API_KEY", "v")]),
        );

        // Restore before asserting so a failure cannot leave the tempdir
        // undeletable.
        std::fs::set_permissions(&path, PermissionsExt::from_mode(0o600)).unwrap();

        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Unresolved,
            "an unreadable artifact must be unresolved, not drift"
        );
        assert!(
            matches!(findings, crate::verify::Findings::Unreachable { .. }),
            "esk did not observe these keys and must not claim they are absent"
        );
    }

    /// The companion to the test above: a *deleted* file is genuinely observed
    /// to hold nothing, so it stays drift. The two must not collapse together.
    #[test]
    fn dotenv_read_back_distinguishes_deleted_from_unreadable() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let config = fixture.config();
        let target = DotenvTarget { config };
        let resolved = make_target(Some("web"), "dev");

        target
            .deploy_batch_state(
                BatchDeployment::without_tombstones(&[make_secret("API_KEY", "v", "G")]),
                &resolved,
            )
            .unwrap();
        let path = config.dotenv_display_path("web", "dev").unwrap();
        std::fs::remove_file(&path).unwrap();

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &resolved),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true },
            "a deleted artifact is observed to hold nothing, so it is drift"
        );
    }
}
