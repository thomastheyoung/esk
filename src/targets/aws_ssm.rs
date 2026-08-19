//! AWS Systems Manager Parameter Store target — deploys secrets via the `aws` CLI.
//!
//! SSM Parameter Store is a key-value store within AWS Systems Manager for
//! configuration data and secrets. Parameters are organized in a hierarchy
//! (e.g. `/{project}/{env}/KEY`) and can be encrypted with KMS (`SecureString`
//! type).
//!
//! CLI: `aws` (AWS CLI v2).
//! Commands: `aws ssm put-parameter` / `aws ssm delete-parameter`.
//!
//! Parameters are created via `--cli-input-json` with the JSON payload on
//! **stdin** to avoid exposing secret values in process arguments. Supports
//! `--region` and `--profile` flags for multi-account setups. The
//! `parameter_type` config field controls the SSM type (default: `SecureString`).

use anyhow::{Context, Result};
use zeroize::Zeroizing;

use std::collections::{BTreeMap, BTreeSet};

use crate::config::{AwsSsmTargetConfig, Config, ResolvedTarget};
use crate::targets::{resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployTarget};
use crate::verify::{Evidence, Fidelity};

pub struct AwsSsmTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a AwsSsmTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl AwsSsmTarget<'_> {
    fn resolve_path(&self, key: &str, target: &ResolvedTarget) -> String {
        let prefix = self
            .target_config
            .path_prefix
            .replace("{project}", &self.config.project)
            .replace("{environment}", &target.environment);
        format!("{prefix}{key}")
    }

    fn base_args(&self) -> Vec<String> {
        crate::targets::aws_base_args(
            self.target_config.region.as_deref(),
            self.target_config.profile.as_deref(),
        )
    }
}

impl DeployTarget for AwsSsmTarget<'_> {
    fn name(&self) -> &'static str {
        "aws_ssm"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Individual
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::aws_preflight(self.runner, &self.base_args())
    }

    fn deploy_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()> {
        let param_path = self.resolve_path(key, target);
        let param_type = &self.target_config.parameter_type;
        let base = self.base_args();

        // Use --cli-input-json via stdin to avoid exposing value in ps output
        let input_json = serde_json::json!({
            "Name": param_path,
            "Value": value,
            "Type": param_type,
            "Overwrite": true,
        });

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec![
            "ssm",
            "put-parameter",
            "--cli-input-json",
            "file:///dev/stdin",
        ];
        args.extend(base.iter().map(String::as_str));
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run(
                "aws",
                &args,
                CommandOpts {
                    stdin: Some(input_json.to_string().into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| format!("failed to run aws ssm put-parameter for {key}"))?;

        output.check("aws ssm put-parameter", key)
    }

    fn verify_fidelity(&self) -> Fidelity {
        Fidelity::Value
    }

    /// Read back via `aws ssm get-parameters --with-decryption`.
    ///
    /// Unlike targets that list everything in one call, SSM addresses each
    /// parameter by its full path, so `keys` is mapped through the same
    /// [`Self::resolve_path`] the deploy uses — the read cannot address a
    /// different parameter than the write.
    ///
    /// `get-parameters` accepts at most 10 names per call, so this chunks. A
    /// failed chunk aborts the whole read rather than contributing nothing:
    /// silently dropping a chunk would return a short map, and every key in it
    /// would be reported as missing from SSM — drift the operator cannot act
    /// on, and worse than admitting the read failed.
    fn read_back(&self, keys: &BTreeSet<String>, target: &ResolvedTarget) -> Result<Evidence> {
        /// AWS caps `get-parameters` at 10 names per request.
        const MAX_NAMES_PER_CALL: usize = 10;

        let base = self.base_args();
        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);

        // Parameters are addressed by path, so the results have to be mapped
        // back to esk's key names.
        let by_path: BTreeMap<String, String> = keys
            .iter()
            .map(|key| (self.resolve_path(key, target), key.clone()))
            .collect();

        let paths: Vec<&String> = by_path.keys().collect();
        let mut values = BTreeMap::new();

        for chunk in paths.chunks(MAX_NAMES_PER_CALL) {
            let mut args: Vec<&str> = vec!["ssm", "get-parameters", "--with-decryption", "--names"];
            args.extend(chunk.iter().map(|p| p.as_str()));
            args.extend(base.iter().map(String::as_str));
            args.extend(flag_parts.iter().map(String::as_str));

            let output = self
                .runner
                .run("aws", &args, CommandOpts::default())
                .context("failed to run aws ssm get-parameters")?;

            if !output.success {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("aws ssm get-parameters failed: {stderr}");
            }

            let json: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("failed to parse ssm get-parameters JSON response")?;

            // `InvalidParameters` lists names that do not exist. They are left
            // out of the map so they report as missing, which is what they are.
            let found = json
                .get("Parameters")
                .and_then(|p| p.as_array())
                .context("ssm get-parameters response had no Parameters array")?;

            for param in found {
                let (Some(name), Some(value)) = (
                    param.get("Name").and_then(|n| n.as_str()),
                    param.get("Value").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if let Some(key) = by_path.get(name) {
                    values.insert(key.clone(), Zeroizing::new(value.to_string()));
                }
            }
        }

        Ok(Evidence::Values(values))
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let param_path = self.resolve_path(key, target);
        let base = self.base_args();

        let flag_parts = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let mut args: Vec<&str> = vec!["ssm", "delete-parameter", "--name", &param_path];
        args.extend(base.iter().map(String::as_str));
        args.extend(flag_parts.iter().map(String::as_str));

        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
            .with_context(|| format!("failed to run aws ssm delete-parameter for {key}"))?;

        output.check("aws ssm delete-parameter", key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};

    fn make_config() -> ConfigFixture {
        let yaml = r#"
project: myapp
environments: [dev, prod]
targets:
  aws_ssm:
    path_prefix: "/{project}/{environment}/"
    region: us-east-1
    env_flags:
      prod: "--no-paginate"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "aws_ssm".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    #[test]
    fn preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: true,
                stdout: b"{}".to_vec(),
                stderr: vec![],
            },
        ]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        assert!(target.preflight().is_ok());
        let calls = runner.take_calls();
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(
            calls[1].args,
            vec!["sts", "get-caller-identity", "--region", "us-east-1"]
        );
    }

    #[test]
    fn preflight_auth_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            CommandOutput {
                success: true,
                stdout: b"2.0.0".to_vec(),
                stderr: vec![],
            },
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"not configured".to_vec(),
            },
        ]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("AWS authentication failed"));
    }

    #[test]
    fn preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("Install from:"));
    }

    #[test]
    fn deploy_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("MY_KEY", "secret_val", &make_target("dev"))
            .unwrap();
        let calls = runner.take_calls();
        assert_eq!(calls[0].program, "aws");
        assert_eq!(
            calls[0].args,
            vec![
                "ssm",
                "put-parameter",
                "--cli-input-json",
                "file:///dev/stdin",
                "--region",
                "us-east-1"
            ]
        );
        // Verify stdin contains the JSON payload
        let stdin = calls[0].stdin.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        assert_eq!(json["Name"], "/myapp/dev/MY_KEY");
        assert_eq!(json["Value"], "secret_val");
        assert_eq!(json["Type"], "SecureString");
        assert_eq!(json["Overwrite"], true);
    }

    #[test]
    fn deploy_with_env_flags() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .deploy_secret("KEY", "val", &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--no-paginate".to_string()));
    }

    #[test]
    fn delete_correct_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: true,
            stdout: vec![],
            stderr: vec![],
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        target.delete_secret("MY_KEY", &make_target("dev")).unwrap();
        let calls = runner.take_calls();
        assert_eq!(
            calls[0].args,
            vec![
                "ssm",
                "delete-parameter",
                "--name",
                "/myapp/dev/MY_KEY",
                "--region",
                "us-east-1"
            ]
        );
    }

    #[test]
    fn delete_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"not found".to_vec(),
        }]);
        let target = AwsSsmTarget {
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
    fn deploy_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![CommandOutput {
            success: false,
            stdout: vec![],
            stderr: b"access denied".to_vec(),
        }]);
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target
            .deploy_secret("KEY", "val", &make_target("dev"))
            .unwrap_err();
        assert!(err.to_string().contains("access denied"));
    }

    #[test]
    fn path_interpolation() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &MockCommandRunner::from_outputs(vec![]),
        };
        let path = target.resolve_path("DB_PASSWORD", &make_target("prod"));
        assert_eq!(path, "/myapp/prod/DB_PASSWORD");
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

    /// Build a `get-parameters` response for the given (path, value) pairs.
    fn params_response(pairs: &[(&str, &str)]) -> Vec<u8> {
        let params: Vec<serde_json::Value> = pairs
            .iter()
            .map(|(name, value)| serde_json::json!({ "Name": name, "Value": value }))
            .collect();
        serde_json::to_vec(&serde_json::json!({
            "Parameters": params,
            "InvalidParameters": [],
        }))
        .unwrap()
    }

    #[test]
    fn aws_ssm_read_back_maps_paths_to_key_names() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&params_response(&[("/myapp/dev/API_KEY", "secret1")]), b"");
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };

        let evidence = target
            .read_back(&verify_keys(&["API_KEY"]), &make_target("dev"))
            .unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("aws_ssm declares Fidelity::Value, so it must return Values");
        };
        // Keyed by esk's key name, not by the SSM path.
        assert_eq!(values["API_KEY"].as_str(), "secret1");

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].args.contains(&"get-parameters".to_string()));
        assert!(
            calls[0].args.contains(&"--with-decryption".to_string()),
            "a SecureString read without decryption returns ciphertext, which \
             would mismatch every value and report universal false drift"
        );
        assert!(calls[0].args.contains(&"/myapp/dev/API_KEY".to_string()));
    }

    #[test]
    fn aws_ssm_read_back_surfaces_wrong_value_as_drift() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&params_response(&[("/myapp/dev/API_KEY", "STALE")]), b"");
        let target = AwsSsmTarget {
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
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Differs);
    }

    #[test]
    fn aws_ssm_read_back_absent_parameter_reports_missing() {
        // SSM returns absent names under `InvalidParameters` rather than as an
        // error, so a deleted parameter must surface as a missing key.
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(
            &serde_json::to_vec(&serde_json::json!({
                "Parameters": [],
                "InvalidParameters": ["/myapp/dev/API_KEY"],
            }))
            .unwrap(),
            b"",
        );
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };

        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&verify_keys(&["API_KEY"]), &make_target("dev")),
            &verify_expected(&[("API_KEY", "v")]),
        );
        let crate::verify::Findings::Values { verdicts, .. } = &findings else {
            panic!("expected value findings");
        };
        assert_eq!(verdicts["API_KEY"], crate::verify::ValueVerdict::Missing);
    }

    /// AWS caps `get-parameters` at 10 names. Reading 12 keys in one call would
    /// have the API reject the request; reading only the first 10 would report
    /// the other two as missing from SSM when they are in fact present.
    #[test]
    fn aws_ssm_read_back_chunks_requests_at_the_api_limit() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();

        let names: Vec<String> = (0..12).map(|i| format!("KEY_{i:02}")).collect();
        let all: Vec<(String, String)> = names
            .iter()
            .map(|k| (format!("/myapp/dev/{k}"), format!("v_{k}")))
            .collect();

        let runner = MockCommandRunner::new().strict();
        // BTreeMap ordering means the first chunk is the first 10 paths.
        let first: Vec<(&str, &str)> = all[..10]
            .iter()
            .map(|(p, v)| (p.as_str(), v.as_str()))
            .collect();
        let second: Vec<(&str, &str)> = all[10..]
            .iter()
            .map(|(p, v)| (p.as_str(), v.as_str()))
            .collect();
        runner.push_success(&params_response(&first), b"");
        runner.push_success(&params_response(&second), b"");

        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let key_set: BTreeSet<String> = names.iter().cloned().collect();
        let evidence = target.read_back(&key_set, &make_target("dev")).unwrap();
        let Evidence::Values(values) = evidence else {
            panic!("expected values");
        };
        assert_eq!(values.len(), 12, "every key must be read, across chunks");
        assert_eq!(values["KEY_11"].as_str(), "v_KEY_11");

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2, "12 keys must be split into two calls");
    }

    /// A failed chunk must abort the read. Contributing nothing would return a
    /// short map, reporting that chunk's keys as missing from SSM.
    #[test]
    fn aws_ssm_read_back_failed_chunk_aborts_rather_than_truncating() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();

        let names: Vec<String> = (0..12).map(|i| format!("KEY_{i:02}")).collect();
        let first: Vec<(String, String)> = names[..10]
            .iter()
            .map(|k| (format!("/myapp/dev/{k}"), "v".to_string()))
            .collect();
        let first_refs: Vec<(&str, &str)> = first
            .iter()
            .map(|(p, v)| (p.as_str(), v.as_str()))
            .collect();

        let runner = MockCommandRunner::new().strict();
        runner.push_success(&params_response(&first_refs), b"");
        runner.push_failure(b"ThrottlingException: Rate exceeded");

        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        let key_set: BTreeSet<String> = names.iter().cloned().collect();
        let findings = crate::verify::compare(
            target.verify_fidelity(),
            target.read_back(&key_set, &make_target("dev")),
            &verify_expected(&[("KEY_00", "v")]),
        );
        assert_eq!(
            findings.assess(),
            crate::verify::Assessment::Unresolved,
            "a partially-read scope must be unresolved, never partial drift"
        );
    }

    #[test]
    fn aws_ssm_read_back_applies_env_flags_and_base_args() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_ssm.as_ref().unwrap();
        let runner = MockCommandRunner::new().strict();
        runner.push_success(&params_response(&[("/myapp/prod/A", "1")]), b"");
        let target = AwsSsmTarget {
            config,
            target_config,
            runner: &runner,
        };
        target
            .read_back(&verify_keys(&["A"]), &make_target("prod"))
            .unwrap();
        let calls = runner.take_calls();
        assert!(calls[0].args.contains(&"--region".to_string()));
        assert!(calls[0].args.contains(&"--no-paginate".to_string()));
    }
}
