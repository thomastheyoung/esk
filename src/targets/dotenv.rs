//! .env file target — writes secrets to a local `.env` file.
//!
//! Not a cloud service — generates standard dotenv files consumed by most
//! frameworks and runtimes (Node.js, Python, Ruby, etc.).
//!
//! Operates in **batch mode**: the entire file is regenerated atomically on
//! every deploy via temp-file-then-rename. Deletions are handled implicitly
//! by omitting the key from the next write. Values containing newlines are
//! rejected by `validate_dotenv_value` before formatting.

use std::collections::BTreeMap;
use std::fmt::Write;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

use crate::config::{Config, ResolvedTarget};
use crate::targets::{DeployMode, DeployOutcome, DeployResult, DeployTarget, SecretValue};

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
        // Path resolution rejects a leaf that is not a regular file, so a
        // directory sitting where the artifact belongs surfaces here rather
        // than as a read error. That is still a definite mismatch: esk did not
        // put it there, and regenerating fails loudly instead of reporting a
        // target it never checked as current.
        let Ok(path) = self.config.resolve_dotenv_path(app, &target.environment) else {
            return Some(false);
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

    fn deploy_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<DeployResult> {
        let Some(app) = &target.app else {
            return secrets
                .iter()
                .map(|s| DeployResult {
                    key: s.key.clone(),
                    outcome: DeployOutcome::Failed(".env target requires an app".to_string()),
                })
                .collect();
        };

        match self.write_dotenv_file(app, &target.environment, secrets) {
            Ok(()) => secrets
                .iter()
                .map(|s| DeployResult {
                    key: s.key.clone(),
                    outcome: DeployOutcome::Success,
                })
                .collect(),
            Err(e) => secrets
                .iter()
                .map(|s| DeployResult {
                    key: s.key.clone(),
                    outcome: DeployOutcome::Failed(e.to_string()),
                })
                .collect(),
        }
    }
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
        let results = target.deploy_batch(&secrets, &make_target(None, "dev"));
        assert!(results.iter().all(|r| !r.outcome.is_success()));
        assert!(results[0]
            .outcome
            .error_message()
            .unwrap()
            .contains("requires an app"));
    }

    #[test]
    fn deploy_batch_writes_env_file() {
        let fixture = make_fixture();
        fixture.create_dir_all("apps/web").unwrap();
        let target = DotenvTarget {
            config: fixture.config(),
        };
        let secrets = vec![make_secret("KEY", "value123", "General")];
        let results = target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
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
        target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
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
        target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
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
        target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
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
        let results = target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
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
        let results = target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
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
        let results = target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
        assert!(results.iter().all(|r| !r.outcome.is_success()));
        assert!(results[0]
            .outcome
            .error_message()
            .unwrap()
            .contains("contains newlines"));
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
        let results = target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
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
        let results = target.deploy_batch(&secrets, &make_target(Some("web"), "dev"));
        assert!(!results[0].outcome.is_success());
        assert!(results[0].outcome.error_message().is_some());
    }
}
