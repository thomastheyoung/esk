//! AWS Lambda deploy target — deploys secrets as Lambda environment variables
//! via the `aws` CLI.
//!
//! Lambda's `update-function-configuration` API replaces the entire environment
//! variable map atomically. To avoid clobbering non-esk vars (`NODE_ENV`,
//! `AWS_REGION`, etc.), this target uses a read-merge-write pattern:
//!
//! 1. `get-function-configuration` → read current env vars + `RevisionId`
//! 2. Overlay esk secrets on top of existing vars
//! 3. `update-function-configuration` with merged map via `--cli-input-json`
//!
//! Uses `RevisionId` as an optimistic concurrency lock. If
//! `ResourceConflictException` occurs, retries the full read-merge-write cycle
//! up to 2 times.
//!
//! CLI: `aws` (AWS CLI v2).
//! Commands: `aws lambda get-function-configuration` /
//!           `aws lambda update-function-configuration`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::config::{AwsLambdaTargetConfig, Config, ResolvedTarget};
use crate::targets::{
    check_command, resolve_env_flags, CommandOpts, CommandRunner, DeployMode, DeployOutcome,
    DeployResult, DeployTarget, SecretValue,
};

/// Maximum number of read-merge-write retries on `ResourceConflictException`.
const MAX_CONFLICT_RETRIES: usize = 2;

pub struct AwsLambdaTarget<'a> {
    pub config: &'a Config,
    pub target_config: &'a AwsLambdaTargetConfig,
    pub runner: &'a dyn CommandRunner,
}

impl AwsLambdaTarget<'_> {
    fn resolve_function_name(&self, env: &str) -> Result<&str> {
        self.target_config
            .function_name
            .get(env)
            .map(String::as_str)
            .with_context(|| format!("no aws_lambda function_name mapping for '{env}'"))
    }

    fn base_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(region) = &self.target_config.region {
            args.push("--region".to_string());
            args.push(region.clone());
        }
        if let Some(profile) = &self.target_config.profile {
            args.push("--profile".to_string());
            args.push(profile.clone());
        }
        args
    }

    /// Read current environment variables and RevisionId from a Lambda function.
    fn read_env_vars(
        &self,
        function_name: &str,
        env_flags: &[String],
    ) -> Result<(BTreeMap<String, String>, Option<String>)> {
        let base = self.base_args();
        let mut args: Vec<&str> = vec![
            "lambda",
            "get-function-configuration",
            "--function-name",
            function_name,
        ];
        args.extend(base.iter().map(String::as_str));
        args.extend(env_flags.iter().map(String::as_str));

        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
            .with_context(|| {
                format!("failed to run aws lambda get-function-configuration for {function_name}")
            })?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "aws lambda get-function-configuration failed for {function_name}: {stderr}"
            );
        }

        let json: serde_json::Value = serde_json::from_slice(&output.stdout)
            .context("failed to parse get-function-configuration JSON response")?;

        let vars = json
            .get("Environment")
            .and_then(|e| e.get("Variables"))
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let revision_id = json
            .get("RevisionId")
            .and_then(|r| r.as_str())
            .map(String::from);

        Ok((vars, revision_id))
    }

    /// Write environment variables to a Lambda function.
    fn write_env_vars(
        &self,
        function_name: &str,
        vars: &BTreeMap<String, String>,
        revision_id: Option<&str>,
        kms_key_arn: Option<&str>,
        env_flags: &[String],
    ) -> Result<()> {
        let base = self.base_args();

        let mut input = serde_json::json!({
            "FunctionName": function_name,
            "Environment": {
                "Variables": vars,
            },
        });

        if let Some(rev) = revision_id {
            input["RevisionId"] = serde_json::json!(rev);
        }
        if let Some(kms) = kms_key_arn {
            input["KMSKeyArn"] = serde_json::json!(kms);
        }

        let mut args: Vec<&str> = vec![
            "lambda",
            "update-function-configuration",
            "--cli-input-json",
            "file:///dev/stdin",
        ];
        args.extend(base.iter().map(String::as_str));
        args.extend(env_flags.iter().map(String::as_str));

        let output = self
            .runner
            .run(
                "aws",
                &args,
                CommandOpts {
                    stdin: Some(input.to_string().into_bytes()),
                    ..Default::default()
                },
            )
            .with_context(|| {
                format!(
                    "failed to run aws lambda update-function-configuration for {function_name}"
                )
            })?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "aws lambda update-function-configuration failed for {function_name}: {stderr}"
            );
        }

        Ok(())
    }

    /// Check if an error is a ResourceConflictException (optimistic lock failure).
    fn is_conflict_error(err: &anyhow::Error) -> bool {
        let msg = err.to_string();
        msg.contains("ResourceConflictException")
    }
}

impl DeployTarget for AwsLambdaTarget<'_> {
    fn name(&self) -> &'static str {
        "aws_lambda"
    }

    fn deploy_mode(&self) -> DeployMode {
        DeployMode::Batch
    }

    fn preflight(&self) -> Result<()> {
        check_command(self.runner, "aws").map_err(|_| {
            anyhow::anyhow!(
                "aws is not installed or not in PATH. Install it from: https://aws.amazon.com/cli/"
            )
        })?;
        let base = self.base_args();
        let mut args: Vec<&str> = vec!["sts", "get-caller-identity"];
        args.extend(base.iter().map(String::as_str));
        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
            .context("failed to run aws sts get-caller-identity")?;
        if !output.success {
            anyhow::bail!("aws is not authenticated. Run: aws configure");
        }
        Ok(())
    }

    fn deploy_secret(&self, _key: &str, _value: &str, _target: &ResolvedTarget) -> Result<()> {
        // Batch target — deploy_batch is the primary method
        Ok(())
    }

    fn delete_secret(&self, key: &str, target: &ResolvedTarget) -> Result<()> {
        let function_name = self.resolve_function_name(&target.environment)?;
        let env_flags = resolve_env_flags(&self.target_config.env_flags, &target.environment);

        let (mut vars, revision_id) = self.read_env_vars(function_name, &env_flags)?;

        if !vars.contains_key(key) {
            // Key doesn't exist on Lambda — nothing to do (idempotent)
            return Ok(());
        }

        vars.remove(key);

        self.write_env_vars(
            function_name,
            &vars,
            revision_id.as_deref(),
            self.target_config.kms_key_arn.as_deref(),
            &env_flags,
        )
    }

    fn deploy_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<DeployResult> {
        let function_name = match self.resolve_function_name(&target.environment) {
            Ok(name) => name,
            Err(e) => {
                return secrets
                    .iter()
                    .map(|s| DeployResult {
                        key: s.key.clone(),
                        outcome: DeployOutcome::Failed(e.to_string()),
                    })
                    .collect();
            }
        };

        let env_flags = resolve_env_flags(&self.target_config.env_flags, &target.environment);
        let kms_key_arn = self.target_config.kms_key_arn.as_deref();

        for attempt in 0..=MAX_CONFLICT_RETRIES {
            // Read current env vars
            let (mut vars, revision_id) = match self.read_env_vars(function_name, &env_flags) {
                Ok(result) => result,
                Err(e) => {
                    return secrets
                        .iter()
                        .map(|s| DeployResult {
                            key: s.key.clone(),
                            outcome: DeployOutcome::Failed(e.to_string()),
                        })
                        .collect();
                }
            };

            // Merge esk secrets on top
            for s in secrets {
                vars.insert(s.key.clone(), s.value.clone());
            }

            // Write merged map
            match self.write_env_vars(
                function_name,
                &vars,
                revision_id.as_deref(),
                kms_key_arn,
                &env_flags,
            ) {
                Ok(()) => {
                    return secrets
                        .iter()
                        .map(|s| DeployResult {
                            key: s.key.clone(),
                            outcome: DeployOutcome::Success,
                        })
                        .collect();
                }
                Err(e) => {
                    if attempt < MAX_CONFLICT_RETRIES && Self::is_conflict_error(&e) {
                        // Retry on conflict
                        continue;
                    }
                    return secrets
                        .iter()
                        .map(|s| DeployResult {
                            key: s.key.clone(),
                            outcome: DeployOutcome::Failed(e.to_string()),
                        })
                        .collect();
                }
            }
        }

        // Should not reach here, but handle gracefully
        secrets
            .iter()
            .map(|s| DeployResult {
                key: s.key.clone(),
                outcome: DeployOutcome::Failed("exceeded max conflict retries".to_string()),
            })
            .collect()
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
  aws_lambda:
    function_name:
      dev: myapp-dev
      prod: myapp-prod
    region: us-east-1
    env_flags:
      prod: "--no-paginate"
"#;
        ConfigFixture::new(yaml).expect("fixture")
    }

    fn make_config_with_kms(dir: &std::path::Path) -> Config {
        let yaml = r#"
project: myapp
environments: [dev, prod]
targets:
  aws_lambda:
    function_name:
      dev: myapp-dev
      prod: myapp-prod
    region: us-east-1
    kms_key_arn: "arn:aws:kms:us-east-1:123456789:key/abc-123"
"#;
        let path = dir.join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_target(env: &str) -> ResolvedTarget {
        ResolvedTarget {
            service: "aws_lambda".to_string(),
            app: None,
            environment: env.to_string(),
        }
    }

    fn make_secret(key: &str, value: &str) -> SecretValue {
        SecretValue {
            key: key.to_string(),
            value: value.to_string(),
            group: "G".to_string(),
        }
    }

    /// Build a mock get-function-configuration JSON response.
    fn get_config_response(vars: &[(&str, &str)], revision_id: &str) -> CommandOutput {
        let variables: serde_json::Value = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::json!(v)))
            .collect::<serde_json::Map<String, serde_json::Value>>()
            .into();

        let json = serde_json::json!({
            "FunctionName": "test-fn",
            "Environment": {
                "Variables": variables,
            },
            "RevisionId": revision_id,
        });

        CommandOutput {
            success: true,
            stdout: json.to_string().into_bytes(),
            stderr: vec![],
        }
    }

    /// Build a mock get-function-configuration response with no environment.
    fn get_config_response_empty() -> CommandOutput {
        let json = serde_json::json!({
            "FunctionName": "test-fn",
            "RevisionId": "rev-1",
        });
        CommandOutput {
            success: true,
            stdout: json.to_string().into_bytes(),
            stderr: vec![],
        }
    }

    fn success_output() -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: b"{}".to_vec(),
            stderr: vec![],
        }
    }

    #[test]
    fn preflight_success() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
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
        let target = AwsLambdaTarget {
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
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
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
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not authenticated"));
    }

    #[test]
    fn preflight_missing_cli() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };
        let err = target.preflight().unwrap_err();
        assert!(err.to_string().contains("aws is not installed"));
    }

    #[test]
    fn deploy_batch_merge() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // get-function-configuration returns existing vars
            get_config_response(
                &[("NODE_ENV", "production"), ("AWS_REGION", "us-east-1")],
                "rev-1",
            ),
            // update-function-configuration succeeds
            success_output(),
        ]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![
            make_secret("API_KEY", "sk-123"),
            make_secret("DB_URL", "postgres://localhost"),
        ];
        let results = target.deploy_batch(&secrets, &make_target("dev"));
        assert!(results.iter().all(|r| r.outcome.is_success()));

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);

        // Verify get call
        assert_eq!(calls[0].args[0], "lambda");
        assert_eq!(calls[0].args[1], "get-function-configuration");
        assert_eq!(calls[0].args[3], "myapp-dev");

        // Verify update call
        assert_eq!(calls[1].args[0], "lambda");
        assert_eq!(calls[1].args[1], "update-function-configuration");

        // Parse the stdin JSON to verify merge
        let stdin = calls[1].stdin.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        let vars = json["Environment"]["Variables"].as_object().unwrap();
        // Existing vars preserved
        assert_eq!(vars["NODE_ENV"], "production");
        assert_eq!(vars["AWS_REGION"], "us-east-1");
        // esk secrets added
        assert_eq!(vars["API_KEY"], "sk-123");
        assert_eq!(vars["DB_URL"], "postgres://localhost");
        // RevisionId passed
        assert_eq!(json["RevisionId"], "rev-1");
    }

    #[test]
    fn deploy_batch_empty_existing() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![get_config_response_empty(), success_output()]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("API_KEY", "sk-123")];
        let results = target.deploy_batch(&secrets, &make_target("dev"));
        assert!(results.iter().all(|r| r.outcome.is_success()));

        let calls = runner.take_calls();
        let stdin = calls[1].stdin.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        let vars = json["Environment"]["Variables"].as_object().unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars["API_KEY"], "sk-123");
    }

    #[test]
    fn deploy_batch_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            get_config_response(&[], "rev-1"),
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"AccessDeniedException".to_vec(),
            },
        ]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let results = target.deploy_batch(&secrets, &make_target("dev"));
        assert!(!results[0].outcome.is_success());
        assert!(results[0]
            .outcome
            .error_message()
            .unwrap()
            .contains("AccessDeniedException"));
    }

    #[test]
    fn deploy_batch_conflict_retry() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            // First attempt: get succeeds
            get_config_response(&[("EXISTING", "val")], "rev-1"),
            // First attempt: update fails with conflict
            CommandOutput {
                success: false,
                stdout: vec![],
                stderr: b"ResourceConflictException: The operation cannot be performed".to_vec(),
            },
            // Second attempt: get succeeds with new revision
            get_config_response(&[("EXISTING", "val")], "rev-2"),
            // Second attempt: update succeeds
            success_output(),
        ]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("API_KEY", "sk-123")];
        let results = target.deploy_batch(&secrets, &make_target("dev"));
        assert!(results.iter().all(|r| r.outcome.is_success()));

        let calls = runner.take_calls();
        // Should have 4 calls: get, update(fail), get, update(success)
        assert_eq!(calls.len(), 4);

        // Second update should have rev-2
        let stdin = calls[3].stdin.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        assert_eq!(json["RevisionId"], "rev-2");
    }

    #[test]
    fn delete_secret_removes_key() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            get_config_response(
                &[("API_KEY", "old-val"), ("NODE_ENV", "production")],
                "rev-1",
            ),
            success_output(),
        ]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        target
            .delete_secret("API_KEY", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        assert_eq!(calls.len(), 2);

        let stdin = calls[1].stdin.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        let vars = json["Environment"]["Variables"].as_object().unwrap();
        assert!(!vars.contains_key("API_KEY"));
        assert_eq!(vars["NODE_ENV"], "production");
    }

    #[test]
    fn delete_secret_key_not_present() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![get_config_response(
            &[("NODE_ENV", "production")],
            "rev-1",
        )]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        target
            .delete_secret("NONEXISTENT", &make_target("dev"))
            .unwrap();

        let calls = runner.take_calls();
        // Only the get call, no update needed
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn function_name_lookup_failure() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        let results = target.deploy_batch(&secrets, &make_target("staging"));
        assert!(!results[0].outcome.is_success());
        assert!(results[0]
            .outcome
            .error_message()
            .unwrap()
            .contains("no aws_lambda function_name mapping"));
    }

    #[test]
    fn kms_key_arn_included() {
        let dir = tempfile::tempdir().unwrap();
        let config = make_config_with_kms(dir.path());
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            get_config_response(&[], "rev-1"),
            success_output(),
        ]);
        let target = AwsLambdaTarget {
            config: &config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        target.deploy_batch(&secrets, &make_target("dev"));

        let calls = runner.take_calls();
        let stdin = calls[1].stdin.as_ref().unwrap();
        let json: serde_json::Value = serde_json::from_slice(stdin).unwrap();
        assert_eq!(
            json["KMSKeyArn"],
            "arn:aws:kms:us-east-1:123456789:key/abc-123"
        );
    }

    #[test]
    fn env_flags_applied() {
        let fixture = make_config();
        let config = fixture.config();
        let target_config = config.targets.aws_lambda.as_ref().unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            get_config_response(&[], "rev-1"),
            success_output(),
        ]);
        let target = AwsLambdaTarget {
            config,
            target_config,
            runner: &runner,
        };

        let secrets = vec![make_secret("KEY", "val")];
        target.deploy_batch(&secrets, &make_target("prod"));

        let calls = runner.take_calls();
        // Both get and update should have --no-paginate
        assert!(calls[0].args.contains(&"--no-paginate".to_string()));
        assert!(calls[1].args.contains(&"--no-paginate".to_string()));
    }
}
