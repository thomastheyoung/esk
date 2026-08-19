//! Render target — deploys env vars via the Render REST API using `curl`.
//!
//! Render.com is a cloud platform for hosting web services, databases, and
//! static sites. Unlike other targets, Render has no CLI — only a REST API.
//!
//! API: `https://api.render.com/v1`
//! Auth: `Authorization: Bearer {api_key}` header.
//! Set: `PUT /services/{serviceId}/env-vars/{envVarKey}` with JSON body.
//! Delete: `DELETE /services/{serviceId}/env-vars/{envVarKey}`.
//!
//! The API key and secret values are passed via curl's `--config -` stdin
//! to avoid exposing them in process argument lists.

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, RenderTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

const BASE_URL: &str = "https://api.render.com/v1";

pub struct RenderTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a RenderTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl RenderTarget<'_> {
    fn api_key(&self) -> Result<String> {
        let api_key = std::env::var(&self.target_config.api_key_env).map_err(|_| {
            anyhow::anyhow!(
                "Render API key not found. Set the {} environment variable.",
                self.target_config.api_key_env
            )
        })?;
        if api_key.chars().any(char::is_control) {
            anyhow::bail!(
                "Render API key in {} contains control characters",
                self.target_config.api_key_env
            );
        }
        Ok(api_key)
    }

    fn resolve_service_id(&self, target: &ResolvedTarget) -> Result<&str> {
        let app = target
            .app
            .as_deref()
            .context("render target requires an app")?;
        self.target_config
            .service_ids
            .get(app)
            .map(String::as_str)
            .with_context(|| format!("no render service_ids mapping for '{app}'"))
    }
}

/// Escape a string for use inside a curl config file value.
/// Backslashes and double quotes must be escaped.
fn curl_config_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// Build a curl config string for `--config -` stdin.
fn build_curl_config(method: &str, url: &str, api_key: &str, body: Option<&str>) -> String {
    use std::fmt::Write;
    let mut config = String::new();
    let _ = writeln!(
        config,
        "header = \"Authorization: Bearer {}\"",
        curl_config_escape(api_key)
    );
    if body.is_some() {
        config.push_str("header = \"Content-Type: application/json\"\n");
    }
    let _ = writeln!(config, "request = \"{method}\"");
    let _ = writeln!(config, "url = \"{}\"", curl_config_escape(url));
    if let Some(body) = body {
        let _ = writeln!(config, "data = \"{}\"", curl_config_escape(body));
    }
    config
}

/// One page of Render's env-var listing.
///
/// `entry_count` is the number of entries the page carried, which is not the
/// same as `values.len()`: entries with no `value` are dropped. Pagination must
/// be decided from `entry_count`, since a full page that parses to fewer values
/// would otherwise look like the last page and truncate the listing.
struct Page {
    entry_count: usize,
    values: BTreeMap<String, Zeroizing<String>>,
}

/// Parse the env-var list response into keys and values.
///
/// Render returns a JSON array of `{"envVar": {"key", "value"}, "cursor"}`
/// wrappers. Entries that are not plain env vars — a secret *file*, whose
/// content lives under a different field — carry no `value`, so they are
/// reported as absent rather than as an empty-string mismatch.
fn parse_env_var_list(stdout: &[u8]) -> Result<Page> {
    let json: serde_json::Value =
        serde_json::from_slice(stdout).context("failed to parse render env-vars JSON response")?;

    let entries = json
        .as_array()
        .context("render env-vars response was not a JSON array")?;

    Ok(Page {
        // The raw count, before the filter below can shrink it.
        entry_count: entries.len(),
        values: entries
            .iter()
            .filter_map(|entry| entry.get("envVar"))
            .filter_map(|env_var| {
                let key = env_var.get("key")?.as_str()?;
                let value = env_var.get("value")?.as_str()?;
                Some((key.to_string(), Zeroizing::new(value.to_string())))
            })
            .collect(),
    })
}

/// The cursor of the last entry in a page, used to request the next one.
fn last_cursor(stdout: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(stdout).ok()?;
    let cursor = json.as_array()?.last()?.get("cursor")?.as_str()?;
    Some(cursor.to_string())
}

/// Check curl output and return a descriptive error on failure.
fn check_curl_output(
    output: &crate::targets::CommandOutput,
    action: &str,
    key: &str,
) -> Result<()> {
    if !output.success {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stdout.is_empty() {
            stderr.to_string()
        } else {
            stdout.to_string()
        };
        anyhow::bail!("render {action} failed for {key}: {detail}");
    }
    Ok(())
}

impl DeployTarget for RenderTarget<'_> {
    fn name(&self) -> &'static str {
        "render"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "curl").context("curl is required for Render API access")?;

        let api_key = self.api_key()?;

        // Use the first service ID to verify authentication
        let first_service_id = self
            .target_config
            .service_ids
            .values()
            .next()
            .context("render target has no service_ids configured")?;

        let url = format!("{BASE_URL}/services/{first_service_id}/env-vars");
        let config_str = build_curl_config("GET", &url, &api_key, None);

        let output = self
            .runner
            .run(
                "curl",
                &["--config", "-", "--silent", "--fail-with-body"],
                CommandOpts {
                    stdin: Some(config_str.into_bytes()),
                    ..Default::default()
                },
            )
            .context("failed to run curl for render preflight")?;

        if !output.success {
            let body = String::from_utf8_lossy(&output.stdout);
            if body.contains("401") || body.contains("Unauthorized") {
                anyhow::bail!(
                    "Render API key is invalid. Check your {} env var.",
                    self.target_config.api_key_env
                );
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("401") || stderr.contains("Unauthorized") {
                anyhow::bail!(
                    "Render API key is invalid. Check your {} env var.",
                    self.target_config.api_key_env
                );
            }
            anyhow::bail!(
                "render preflight failed: {}{}",
                body,
                if stderr.is_empty() {
                    String::new()
                } else {
                    format!(" (stderr: {stderr})")
                }
            );
        }

        Ok(())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let service_id = self.resolve_service_id(target)?;
        let api_key = self.api_key()?;

        let url = format!("{BASE_URL}/services/{service_id}/env-vars/{key}");
        let json_value = serde_json::to_string(value).expect("string is always valid JSON");
        let body = format!("{{\"value\":{json_value}}}");
        let config_str = build_curl_config("PUT", &url, &api_key, Some(&body));

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["--config", "-", "--silent", "--fail-with-body"];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run(
                "curl",
                &args,
                CommandOpts {
                    stdin: Some(config_str.into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run curl for render deploy {key}"))?;

        check_curl_output(&output, "deploy", key)
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `GET /services/{id}/env-vars`, the same request preflight
    /// already makes and discards.
    ///
    /// The listing is paginated. `limit=100` is Render's maximum page size, and
    /// a full page means there may be more: rather than return a short map —
    /// which `compare` would report as keys missing from the service — this
    /// follows the cursor until a partial page arrives.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        const PAGE_SIZE: usize = 100;
        // Bounds the loop if a provider ever returns a non-advancing cursor.
        const MAX_PAGES: usize = 100;

        let service_id = self.resolve_service_id(target)?;
        let api_key = self.api_key()?;
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);

        let mut all = BTreeMap::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_PAGES {
            let url = match &cursor {
                Some(cursor) => format!(
                    "{BASE_URL}/services/{service_id}/env-vars?limit={PAGE_SIZE}&cursor={cursor}"
                ),
                None => format!("{BASE_URL}/services/{service_id}/env-vars?limit={PAGE_SIZE}"),
            };
            let config_str = build_curl_config("GET", &url, &api_key, None);

            let mut args: Vec<&str> = vec!["--config", "-", "--silent", "--fail-with-body"];
            args.extend(flag_parts.iter().map(String::as_str));

            let output = self
                .runner
                .run(
                    "curl",
                    &args,
                    CommandOpts {
                        stdin: Some(config_str.into_bytes()),
                        ..Default::default()
                    },
                )
                .context("failed to run curl for render read-back")?;

            // A failed page is an incomplete read, not an empty service.
            check_curl_output(&output, "read-back", service_id)?;

            let page = parse_env_var_list(&output.stdout)?;
            // Fullness comes from the raw entry count, never from the parsed
            // map: entries without a value are dropped, so a full page can
            // parse to fewer values and would otherwise end the loop early.
            let entry_count = page.entry_count;
            all.extend(page.values);

            if entry_count < PAGE_SIZE {
                return Ok(Evidence::Values(all));
            }

            cursor = last_cursor(&output.stdout);
            if cursor.is_none() {
                // A full page with no cursor to continue from: the listing may
                // be truncated and there is no way to finish it.
                anyhow::bail!(
                    "render returned a full page of env vars with no cursor to continue from"
                );
            }
        }

        anyhow::bail!("render env-vars listing did not terminate after {MAX_PAGES} pages")
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let service_id = self.resolve_service_id(target)?;
        let api_key = self.api_key()?;

        let url = format!("{BASE_URL}/services/{service_id}/env-vars/{key}");
        let config_str = build_curl_config("DELETE", &url, &api_key, None);

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["--config", "-", "--silent", "--fail-with-body"];
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run(
                "curl",
                &args,
                CommandOpts {
                    stdin: Some(config_str.into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run curl for render delete {key}"))?;

        check_curl_output(&output, "delete", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_config(api_key_env: &str) -> ConfigFixture {
        let yaml = format!(
            r#"
project: x
environments: [dev, prod]
apps:
  web:
    path: apps/web
targets:
  render:
    service_ids:
      web: srv-abc123def456
    api_key_env: {api_key_env}
    env_flags:
      prod: "--proxy http://proxy:8080"
"#
        );
        ConfigFixture::new(&yaml).unwrap()
    }

    fn make_target(app: Option<&str>, env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "render".to_string(),
            app: app.map(String::from),
            environment: env.to_string(),
        }
    }

    /// Generate a unique env var name per test to avoid parallel test races.
    fn unique_api_key_env(test_name: &str) -> String {
        format!("RENDER_TEST_KEY_{}", test_name.to_uppercase())
    }

    #[test]
    fn render_preflight_success() {
        let env_name = unique_api_key_env("preflight_success");
        std::env::set_var(&env_name, "rnd_test_key_123");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"curl 7.80.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"[{\"key\":\"TEST\",\"value\":\"val\"}]".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        // Second call should be the auth check via curl --config
        assert_eq!(
            calls[1].args,
            vec!["--config", "-", "--silent", "--fail-with-body"]
        );
        let stdin = String::from_utf8(calls[1].stdin.clone().unwrap()).unwrap();
        assert!(stdin.contains("Authorization: Bearer rnd_test_key_123"));
        assert!(stdin.contains("srv-abc123def456"));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_preflight_missing_curl() {
        let env_name = unique_api_key_env("preflight_missing_curl");
        std::env::set_var(&env_name, "key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("curl is required for Render API access"));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_preflight_auth_failure() {
        let env_name = unique_api_key_env("preflight_auth_failure");
        std::env::set_var(&env_name, "bad_key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"curl 7.80.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: b"401 Unauthorized".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("API key is invalid"));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_deploy_builds_correct_curl_config() {
        let env_name = unique_api_key_env("deploy_correct");
        std::env::set_var(&env_name, "rnd_deploy_key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["--config", "-", "--silent", "--fail-with-body"]
        );
        let stdin = String::from_utf8(calls[0].stdin.clone().unwrap()).unwrap();
        assert!(stdin.contains("Authorization: Bearer rnd_deploy_key"));
        assert!(stdin.contains("Content-Type: application/json"));
        assert!(stdin.contains("request = \"PUT\""));
        assert!(stdin.contains("srv-abc123def456/env-vars/MY_KEY"));
        // JSON body is curl-config-escaped: quotes become \"
        assert!(stdin.contains(r#"data = "{\"value\":\"secret_val\"}"#));
        // Value NOT in args
        assert!(!calls[0].args.iter().any(|a| a.contains("secret_val")));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_deploy_with_env_flags() {
        let env_name = unique_api_key_env("deploy_env_flags");
        std::env::set_var(&env_name, "key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "--config",
                "-",
                "--silent",
                "--fail-with-body",
                "--proxy",
                "http://proxy:8080"
            ]
        );
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_deploy_escapes_special_chars() {
        let env_name = unique_api_key_env("deploy_escapes");
        std::env::set_var(&env_name, "key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret(
                "KEY",
                "val with \"quotes\" and \\backslash",
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let calls = runner.take_calls();
        let stdin = String::from_utf8(calls[0].stdin.clone().unwrap()).unwrap();
        // The JSON value should have escaped quotes, then curl config escapes those
        assert!(stdin.contains("val with"));
        // Verify the config doesn't have unescaped quotes that would break curl config parsing
        // The data line should have the JSON-then-curl-config-escaped value
        assert!(stdin.contains("data = "));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_requires_app() {
        let env_name = unique_api_key_env("requires_app");
        std::env::set_var(&env_name, "key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(None, "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("requires an app"));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_unknown_service_mapping() {
        let env_name = unique_api_key_env("unknown_service");
        std::env::set_var(&env_name, "key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("api"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("no render service_ids mapping"));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_nonzero_exit() {
        let env_name = unique_api_key_env("nonzero_exit");
        std::env::set_var(&env_name, "key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: b"Internal Server Error".to_vec(),
            stderr: vec![],
        }]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("render deploy failed"));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_delete_correct_config() {
        let env_name = unique_api_key_env("delete_correct");
        std::env::set_var(&env_name, "rnd_del_key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .delete_secret("MY_KEY", &make_target(Some("web"), "dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["--config", "-", "--silent", "--fail-with-body"]
        );
        let stdin = String::from_utf8(calls[0].stdin.clone().unwrap()).unwrap();
        assert!(stdin.contains("Authorization: Bearer rnd_del_key"));
        assert!(stdin.contains("request = \"DELETE\""));
        assert!(stdin.contains("srv-abc123def456/env-vars/MY_KEY"));
        // No Content-Type or data for delete
        assert!(!stdin.contains("Content-Type"));
        assert!(!stdin.contains("data = "));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn render_delete_failure() {
        let env_name = unique_api_key_env("delete_failure");
        std::env::set_var(&env_name, "key");
        let fixture = make_config(&env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: b"Not Found".to_vec(),
            stderr: vec![],
        }]);
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target(Some("web"), "dev"))
            .unwrap_err();
        assert!(err.to_string().contains("render delete failed"));
        std::env::remove_var(&env_name);
    }

    #[test]
    fn curl_config_escape_special_chars() {
        assert_eq!(curl_config_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(curl_config_escape("line1\r\nline2"), "line1\\r\\nline2");
        assert_eq!(curl_config_escape("normal"), "normal");
        assert_eq!(curl_config_escape(""), "");
    }

    #[test]
    fn api_key_rejects_control_characters() {
        let env_name = "RENDER_TEST_API_KEY_CONTROL";
        let fixture = make_config(env_name);
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::new();
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };
        std::env::set_var(env_name, "valid\nheader = \"X-Evil: yes\"");

        let err = target.api_key().unwrap_err();
        assert!(err.to_string().contains("contains control characters"));
        std::env::remove_var(env_name);
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

    /// Build a Render env-vars list page.
    fn env_var_page(entries: &[(&str, &str)]) -> Vec<u8> {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .enumerate()
            .map(|(i, (k, v))| {
                serde_json::json!({
                    "envVar": { "key": k, "value": v },
                    "cursor": format!("c{i}"),
                })
            })
            .collect();
        serde_json::to_vec(&items).unwrap()
    }

    #[test]
    fn render_read_back_returns_listed_values() {
        let fixture = make_config("RENDER_KEY_RB1");
        std::env::set_var("RENDER_KEY_RB1", "rnd_test");
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&env_var_page(&[("API_KEY", "secret1")]), b"");
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("render declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        let stdin = String::from_utf8(calls[0].stdin.clone().unwrap()).unwrap();
        assert!(stdin.contains("request = \"GET\""));
        assert!(stdin.contains("srv-abc123def456/env-vars"));
        std::env::remove_var("RENDER_KEY_RB1");
    }

    #[test]
    fn render_read_back_surfaces_wrong_value_as_drift() {
        let fixture = make_config("RENDER_KEY_RB2");
        std::env::set_var("RENDER_KEY_RB2", "rnd_test");
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&env_var_page(&[("API_KEY", "STALE")]), b"");
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "current")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Differs);
        std::env::remove_var("RENDER_KEY_RB2");
    }

    #[test]
    fn render_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_config("RENDER_KEY_RB3");
        std::env::set_var("RENDER_KEY_RB3", "rnd_test");
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"503 Service Unavailable");
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target(Some("web"), "dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
        std::env::remove_var("RENDER_KEY_RB3");
    }

    #[test]
    fn render_read_back_follows_pagination_cursor() {
        // A full page means there may be more. Returning only the first page
        // would report every later key as missing from the service — drift the
        // operator would chase on secrets that are in fact correct.
        let fixture = make_config("RENDER_KEY_RB4");
        std::env::set_var("RENDER_KEY_RB4", "rnd_test");
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();

        let full_page: Vec<(String, String)> = (0..100)
            .map(|i| (format!("KEY_{i:03}"), format!("v{i}")))
            .collect();
        let page_refs: Vec<(&str, &str)> = full_page
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let runner = MockCommandRunner::new().strict();
        runner.push_success(&env_var_page(&page_refs), b"");
        runner.push_success(&env_var_page(&[("LAST_KEY", "final")]), b"");
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(
                &verify_keys(&["LAST_KEY"]),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values");
        };
        assert_eq!(values.len(), 101);
        assert_eq!(values["LAST_KEY"].as_str(), "final");

        let calls = runner.take_calls();
        assert_eq!(
            calls.len(),
            2,
            "a full page must be followed by another request"
        );
        let second = String::from_utf8(calls[1].stdin.clone().unwrap()).unwrap();
        assert!(
            second.contains("cursor=c99"),
            "second page must resume from the last cursor: {second}"
        );
        std::env::remove_var("RENDER_KEY_RB4");
    }

    #[test]
    fn render_read_back_full_page_without_cursor_is_unreachable() {
        let fixture = make_config("RENDER_KEY_RB5");
        std::env::set_var("RENDER_KEY_RB5", "rnd_test");
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();

        // A full page whose entries carry no cursor cannot be continued.
        let items: Vec<serde_json::Value> = (0..100)
            .map(|i| serde_json::json!({ "envVar": { "key": format!("K{i}"), "value": "v" } }))
            .collect();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&serde_json::to_vec(&items).unwrap(), b"");
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };

        // Matched rather than `unwrap_err`: `Evidence` intentionally has no
        // `Debug` impl, since one would format secret values into panics.
        let Err(err) = target.read_back(&verify_keys(&["K0"]), &make_target(Some("web"), "dev"))
        else {
            panic!("a full page with no cursor must not be reported as a complete read");
        };
        assert!(err.to_string().contains("no cursor"), "got: {err}");
        std::env::remove_var("RENDER_KEY_RB5");
    }

    #[test]
    fn render_read_back_skips_entries_without_a_value() {
        // Secret *files* appear in the same listing but carry no `value`.
        // Treating them as empty strings would invent a mismatch.
        let items = serde_json::json!([
            { "envVar": { "key": "REAL", "value": "v" }, "cursor": "c0" },
            { "envVar": { "key": "FILE_ONLY" }, "cursor": "c1" },
        ]);
        let parsed = parse_env_var_list(&serde_json::to_vec(&items).unwrap()).unwrap();
        assert_eq!(parsed.values.len(), 1);
        assert_eq!(parsed.values["REAL"].as_str(), "v");
        // The dropped entry still counts toward the page's fullness.
        assert_eq!(parsed.entry_count, 2);
    }

    #[test]
    fn render_read_back_unparseable_response_is_an_error() {
        assert!(parse_env_var_list(b"not json").is_err());
        assert!(parse_env_var_list(b"{}").is_err());
    }

    #[test]
    fn render_read_back_full_page_containing_a_secret_file_still_paginates() {
        // `parse_env_var_list` drops entries with no `value` (secret files), so
        // a FULL page of 100 entries can parse to 99. Deciding "last page" from
        // the parsed count would stop early and silently truncate the listing —
        // every later key would then be reported as missing from the service.
        let fixture = make_config("RENDER_KEY_RB6");
        std::env::set_var("RENDER_KEY_RB6", "rnd_test");
        let config = fixture.config();
        let target_config = config.targets.render.as_ref().unwrap();

        // 99 normal vars + 1 secret file = 100 raw entries, 99 parsed.
        let mut items: Vec<serde_json::Value> = (0..99)
            .map(|i| {
                serde_json::json!({
                    "envVar": { "key": format!("KEY_{i:03}"), "value": "v" },
                    "cursor": format!("c{i}"),
                })
            })
            .collect();
        items.push(serde_json::json!({
            "envVar": { "key": "SECRET_FILE" },
            "cursor": "c99",
        }));

        let runner = MockCommandRunner::new().strict();
        runner.push_success(&serde_json::to_vec(&items).unwrap(), b"");
        runner.push_success(&env_var_page(&[("LAST_KEY", "final")]), b"");
        let target = RenderTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(
                &verify_keys(&["LAST_KEY"]),
                &make_target(Some("web"), "dev"),
            )
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values")
        };
        assert!(
            values.contains_key("LAST_KEY"),
            "a full raw page must be followed, even when parsing drops an entry"
        );
        std::env::remove_var("RENDER_KEY_RB6");
    }
}
