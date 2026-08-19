//! Netlify target — deploys environment variables via the `netlify` CLI.
//!
//! Netlify is a web hosting and automation platform for modern web projects.
//! Environment variables are available during builds and in Netlify Functions
//! (serverless).
//!
//! CLI: `netlify` (Netlify's official CLI).
//! Commands: `netlify env:set` / `netlify env:unset`.
//!
//! The Netlify CLI does **not** support stdin or file input for secret values,
//! so they are passed as command-line arguments (visible in `ps` output).
//! Supports an optional `--site` flag to target a specific site.

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{Config, NetlifyTargetConfig, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget,
};
use crate::verify::{Evidence, Fidelity};

pub struct NetlifyTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a NetlifyTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl DeployTarget for NetlifyTarget<'_> {
    fn name(&self) -> &'static str {
        "netlify"
    }

    fn passes_value_as_cli_arg(&self) -> bool {
        true
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "netlify")
            .context("Install with: npm install -g netlify-cli")?;
        let output = self
            .runner
            .run("netlify", &["status"], CommandOpts::default())
            .context("failed to run netlify status")?;
        if !output.success {
            anyhow::bail!("netlify is not linked. Run: netlify link");
        }
        Ok(())
    }

    // SECURITY: netlify CLI has no stdin/file support for `env:set`. It has `env:import` but with
    // different semantics (replaces all vars). Secret values are exposed in process arguments
    // (visible via `ps aux`). No workaround available.
    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:set", key, value];
        if let Some(site) = &self.target_config.site {
            args.push("--site");
            args.push(site);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("netlify", &args, CommandOpts::default())
            .with_context(|| format!("failed to run netlify env:set for {key}"))?
            .check("netlify env:set", key)
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `netlify env:list --json`, which returns resolved values.
    ///
    /// `--context all` is passed explicitly and deliberately. `env:set` with no
    /// context flag writes to every context, but `env:list` defaults to the
    /// `dev` context alone — so relying on the defaults would have the read
    /// address a narrower scope than the write, and every variable set outside
    /// `dev` would read back missing. The two commands must span the same
    /// contexts or the comparison is meaningless.
    ///
    /// esk does not map its environment names onto Netlify's context names
    /// (`production`, `deploy-preview`, `branch-deploy`), and inventing a
    /// mapping would be a guess; spanning all contexts is what `env:set`
    /// actually does. A user who scopes deploys with `env_flags` should set the
    /// matching context there, and it is appended after this default.
    ///
    /// "Resolved" includes values set in `netlify.toml` as well as ones esk
    /// deployed, so a key can be present without esk having written it. That
    /// still compares correctly: if the resolved value differs from the store,
    /// the site is not serving what esk holds, which is drift worth reporting.
    fn read_back(&self, _keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:list", "--json", "--context", "all"];
        if let Some(site) = &self.target_config.site {
            args.push("--site");
            args.push(site);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("netlify", &args, CommandOpts::default())
            .context("failed to run netlify env:list")?;

        // A failed listing is an incomplete read, never an empty site.
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("netlify env:list failed: {stderr}");
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse netlify env:list JSON response")?;
        let object = json
            .as_object()
            .context("netlify env:list response was not a JSON object")?;

        let mut values = BTreeMap::new();
        for (key, value) in object {
            // A non-string value means the response is not the flat shape this
            // parser expects — per-context objects appear when the listing is
            // not resolved to one context. Dropping the key would report it
            // missing from the site, which is false; an unexpected shape is an
            // incomplete read.
            let value = value.as_str().with_context(|| {
                format!("netlify env:list returned a non-string value for '{key}'")
            })?;
            values.insert(key.clone(), Zeroizing::new(value.to_string()));
        }

        Ok(Evidence::Values(values))
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["env:unset", key];
        if let Some(site) = &self.target_config.site {
            args.push("--site");
            args.push(site);
        }
        args.extend(flag_parts.iter().map(String::as_str));

        self.runner
            .run("netlify", &args, CommandOpts::default())
            .with_context(|| format!("failed to run netlify env:unset for {key}"))?
            .check("netlify env:unset", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    const NETLIFY_YAML: &str = r#"
project: x
environments: [dev, prod]
targets:
  netlify:
    env_flags:
      prod: "--context production"
"#;

    const NETLIFY_YAML_WITH_SITE: &str = r#"
project: x
environments: [dev, prod]
targets:
  netlify:
    site: my-site-id
    env_flags:
      prod: "--context production"
"#;

    fn make_fixture(with_site: bool) -> ConfigFixture {
        let yaml = if with_site {
            NETLIFY_YAML_WITH_SITE
        } else {
            NETLIFY_YAML
        };
        ConfigFixture::new(yaml).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "netlify".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn netlify_preflight_success() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"linked".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[1].args, vec!["status"]);
    }

    #[test]
    fn netlify_preflight_not_linked() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"1.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not linked".to_vec(),
            },
        ]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("netlify is not linked"));
    }

    #[test]
    fn netlify_preflight_missing_cli() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err
            .to_string()
            .contains("Install with: npm install -g netlify-cli"));
    }

    #[test]
    fn netlify_deploy_correct_args() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "netlify");
        assert_eq!(calls[0].args, vec!["env:set", "MY_KEY", "secret_val"]);
    }

    #[test]
    fn netlify_deploy_with_site() {
        let fixture = make_fixture(true);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["env:set", "KEY", "val", "--site", "my-site-id"]
        );
    }

    #[test]
    fn netlify_deploy_with_env_flags() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["env:set", "KEY", "val", "--context", "production"]
        );
    }

    #[test]
    fn netlify_delete_correct_args() {
        let fixture = make_fixture(true);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec!["env:unset", "MY_KEY", "--site", "my-site-id"]
        );
    }

    #[test]
    fn netlify_delete_failure() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .delete_secret("KEY", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn netlify_nonzero_exit() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"auth error".to_vec(),
        }]);
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("auth error"));
    }

    fn verify_keys(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_string()).collect()
    }

    fn verify_expected(
        pairs: &[(&str, &str)],
    ) -> std::collections::BTreeMap<String, Zeroizing<String>> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), Zeroizing::new((*v).to_string())))
            .collect()
    }

    fn env_list_json(pairs: &[(&str, &str)]) -> Vec<u8> {
        let map: serde_json::Map<String, serde_json::Value> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::json!(v)))
            .collect();
        serde_json::to_vec(&map).unwrap()
    }

    #[test]
    fn netlify_read_back_returns_values() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&env_list_json(&[("API_KEY", "secret1")]), b"");
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target("dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("netlify declares Fidelity::Value, so it must return Values");
        };
        assert_eq!(values["API_KEY"].as_str(), "secret1");

        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "netlify");
        assert!(calls[0].args.contains(&"env:list".to_string()));
        assert!(calls[0].args.contains(&"--json".to_string()));
    }

    #[test]
    fn netlify_read_back_surfaces_wrong_value_as_drift() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&env_list_json(&[("API_KEY", "STALE")]), b"");
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "current")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Resolved { drifted: true }
        );
    }

    #[test]
    fn netlify_read_back_failure_is_unreachable_not_empty() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_failure(b"Not authorized");
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        assert_eq!(findings.assess(), crate::verify::Assessment::Unresolved);
    }

    /// `env:set` writes to every context but `env:list` defaults to `dev`
    /// alone, so the read must ask for all contexts or it addresses a narrower
    /// scope than the write — reporting every non-dev variable as missing.
    #[test]
    fn netlify_read_back_spans_the_same_contexts_the_deploy_writes() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&env_list_json(&[("A", "1")]), b"");
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .read_back(&verify_keys(&["A"]), &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        let args = &calls[0].args;
        let context_at = args.iter().position(|a| a == "--context");
        assert!(
            context_at.is_some(),
            "the read must name its context explicitly rather than take env:list's default"
        );
        assert_eq!(args[context_at.unwrap() + 1], "all");
    }

    /// A per-context object where a plain value was expected means the response
    /// is not the shape this parser reads. Dropping the key would report it
    /// missing from the site, which is false.
    #[test]
    fn netlify_read_back_non_string_value_is_an_incomplete_read() {
        let fixture = make_fixture(false);
        let config = fixture.config();
        let target_config = config.targets.netlify.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(br#"{"API_KEY":{"production":"p","dev":"d"}}"#, b"");
        let target = NetlifyTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "p")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Unresolved,
            "an unexpected response shape must not yield a value verdict"
        );
    }
}
