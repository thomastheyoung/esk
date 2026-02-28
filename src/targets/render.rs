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

use crate::config::{Config, RenderTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};

const BASE_URL: &str = "https://api.render.com/v1";

pub struct RenderTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a RenderTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl RenderTarget<'_> {
    fn api_key(&self) -> Result<String> {
        std::env::var(&self.target_config.api_key_env).map_err(|_| {
            anyhow::anyhow!(
                "Render API key not found. Set the {} environment variable.",
                self.target_config.api_key_env
            )
        })
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
    s.replace('\\', "\\\\").replace('"', "\\\"")
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
        check_command(self.runner, "curl").map_err(|_| {
            anyhow::anyhow!("curl is not installed or not in PATH. Install it and try again.")
        })?;

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
        assert!(err.to_string().contains("curl is not installed"));
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
        assert_eq!(curl_config_escape("normal"), "normal");
        assert_eq!(curl_config_escape(""), "");
    }
}
