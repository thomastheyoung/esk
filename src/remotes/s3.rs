//! S3-compatible storage remote — syncs secrets via the `aws` CLI's S3 commands.
//!
//! Works with any S3-compatible object store: AWS S3, Cloudflare R2, MinIO,
//! DigitalOcean Spaces, Backblaze B2, etc. Secrets are stored as a single JSON
//! file per environment in a bucket.
//!
//! CLI: `aws` (AWS CLI v2).
//! Commands: `aws s3 cp - s3://...` (push via stdin) / `aws s3 cp s3://... -` (pull to stdout).
//!
//! The store payload is serialized as JSON and streamed via **stdin**. Supports
//! cleartext or encrypted format (same AES-256-GCM as the local store). The
//! `--endpoint-url` flag enables non-AWS S3-compatible providers. Supports
//! `--region` and `--profile` for multi-account setups.

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::config::{CloudFileFormat, Config, S3RemoteConfig};
use crate::store::{SecretStore, StorePayload};
use crate::targets::{CommandOpts, CommandRunner};

use super::SyncRemote;

pub struct S3Remote<'a> {
    config: &'a Config,
    remote_config: S3RemoteConfig,
    runner: &'a dyn CommandRunner,
}

impl<'a> S3Remote<'a> {
    pub fn new(
        config: &'a Config,
        remote_config: S3RemoteConfig,
        runner: &'a dyn CommandRunner,
    ) -> Self {
        Self {
            config,
            remote_config,
            runner,
        }
    }

    /// Build base args for --region, --profile, --endpoint-url.
    fn base_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(region) = &self.remote_config.region {
            args.push("--region".to_string());
            args.push(region.clone());
        }
        if let Some(profile) = &self.remote_config.profile {
            args.push("--profile".to_string());
            args.push(profile.clone());
        }
        if let Some(endpoint) = &self.remote_config.endpoint {
            args.push("--endpoint-url".to_string());
            args.push(endpoint.clone());
        }
        args
    }

    /// Build the S3 URI for a given environment.
    fn s3_uri(&self, env: &str) -> String {
        let ext = match self.remote_config.format {
            CloudFileFormat::Encrypted => "enc",
            CloudFileFormat::Cleartext => "json",
        };
        let prefix = self.remote_config.prefix.as_deref().unwrap_or("");
        if prefix.is_empty() {
            format!("s3://{}/secrets-{env}.{ext}", self.remote_config.bucket)
        } else {
            let prefix = prefix.trim_end_matches('/');
            format!(
                "s3://{}/{prefix}/secrets-{env}.{ext}",
                self.remote_config.bucket
            )
        }
    }

    /// Build a per-env StorePayload with bare keys.
    fn env_payload(payload: &StorePayload, env: &str) -> StorePayload {
        let suffix = format!(":{env}");
        let bare: BTreeMap<String, String> = payload
            .secrets
            .iter()
            .filter_map(|(k, v)| {
                k.strip_suffix(&suffix)
                    .map(|bare| (bare.to_string(), v.clone()))
            })
            .collect();
        let version = payload
            .env_versions
            .get(env)
            .copied()
            .unwrap_or(payload.version);
        let mut env_last_changed_at = BTreeMap::new();
        if let Some(ts) = payload.env_last_changed_at(env) {
            env_last_changed_at.insert(env.to_string(), ts.to_string());
        }
        StorePayload {
            secrets: bare,
            version,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at,
        }
    }

    /// Convert bare keys back to composite keys.
    fn bare_to_composite(bare: &BTreeMap<String, String>, env: &str) -> BTreeMap<String, String> {
        bare.iter()
            .map(|(k, v)| (format!("{k}:{env}"), v.clone()))
            .collect()
    }
}

impl SyncRemote for S3Remote<'_> {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "aws").map_err(|_| {
            anyhow::anyhow!(
                "AWS CLI (aws) is not installed or not in PATH. Install it from: https://aws.amazon.com/cli/"
            )
        })?;

        let mut args: Vec<String> = vec!["sts".to_string(), "get-caller-identity".to_string()];
        args.extend(self.base_args());
        let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();

        let output = self
            .runner
            .run("aws", &args_ref, CommandOpts::default())
            .context("failed to run aws sts get-caller-identity")?;
        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("AWS authentication failed: {stderr}");
        }

        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        let env_payload = Self::env_payload(payload, env);

        if env_payload.secrets.is_empty() {
            return Ok(());
        }

        let s3_uri = self.s3_uri(env);
        let base_args = self.base_args();

        let content = match self.remote_config.format {
            CloudFileFormat::Encrypted => {
                let store = SecretStore::open(&self.config.root)?;
                let json = serde_json::to_string(&env_payload)
                    .context("failed to serialize env payload")?;
                store.encrypt_raw(&json)?
            }
            CloudFileFormat::Cleartext => serde_json::to_string_pretty(&env_payload)
                .context("failed to serialize env payload")?,
        };

        let mut args = vec![
            "s3".to_string(),
            "cp".to_string(),
            "-".to_string(),
            s3_uri.clone(),
        ];
        args.extend(base_args);
        let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();

        let output = self
            .runner
            .run(
                "aws",
                &args_ref,
                CommandOpts {
                    stdin: Some(content.into_bytes()),
                    ..Default::default()
                },
            )
            .context("failed to run aws s3 cp")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("aws s3 cp upload failed: {stderr}");
        }

        Ok(())
    }

    fn pull(&self, _config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let s3_uri = self.s3_uri(env);
        let base_args = self.base_args();

        let mut args = vec![
            "s3".to_string(),
            "cp".to_string(),
            s3_uri.clone(),
            "-".to_string(),
        ];
        args.extend(base_args);
        let args_ref: Vec<&str> = args.iter().map(std::string::String::as_str).collect();

        let output = self
            .runner
            .run("aws", &args_ref, CommandOpts::default())
            .context("failed to run aws s3 cp")?;

        if !output.success {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("NoSuchKey")
                || stderr.contains("404")
                || stderr.contains("does not exist")
            {
                return Ok(None);
            }
            anyhow::bail!("aws s3 cp download failed: {stderr}");
        }

        let content = String::from_utf8(output.stdout).context("S3 response is not valid UTF-8")?;
        let content = content.trim();

        if content.is_empty() {
            return Ok(None);
        }

        let payload = match self.remote_config.format {
            CloudFileFormat::Encrypted => {
                let store = SecretStore::open(&self.config.root)?;
                store.decrypt_raw(content)?
            }
            CloudFileFormat::Cleartext => {
                serde_json::from_str(content).context("failed to parse secrets JSON from S3")?
            }
        };

        Ok(Some((
            Self::bare_to_composite(&payload.secrets, env),
            payload.version,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ErrorCommandRunner, MockCommandRunner};

    type RunnerCall = (String, Vec<String>);

    fn calls(runner: &MockCommandRunner) -> Vec<RunnerCall> {
        runner
            .calls()
            .into_iter()
            .map(|call| (call.program, call.args))
            .collect()
    }

    fn make_config(yaml: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        std::mem::forget(dir);
        config
    }

    fn ok_output(stdout: &[u8]) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    fn fail_output(stderr: &[u8]) -> CommandOutput {
        CommandOutput {
            success: false,
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn preflight_success() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"aws-cli/2.13.0"),
            ok_output(b"{\"Account\": \"123456789012\"}"),
        ]);
        let remote = S3Remote::new(&config, remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, vec!["--version"]);
        assert_eq!(calls[1].1, vec!["sts", "get-caller-identity"]);
    }

    #[test]
    fn preflight_success_with_profile_region() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
    region: us-west-2
    profile: myprofile
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"aws-cli/2.13.0"),
            ok_output(b"{\"Account\": \"123456789012\"}"),
        ]);
        let remote = S3Remote::new(&config, remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = calls(&runner);
        assert_eq!(calls.len(), 2);
        let sts_args = &calls[1].1;
        assert!(sts_args.contains(&"sts".to_string()));
        assert!(sts_args.contains(&"get-caller-identity".to_string()));
        assert!(sts_args.contains(&"--region".to_string()));
        assert!(sts_args.contains(&"us-west-2".to_string()));
        assert!(sts_args.contains(&"--profile".to_string()));
        assert!(sts_args.contains(&"myprofile".to_string()));
    }

    #[test]
    fn preflight_auth_failure() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"aws-cli/2.13.0"),
            fail_output(b"Unable to locate credentials"),
        ]);
        let remote = S3Remote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("AWS authentication failed"));
    }

    #[test]
    fn preflight_aws_not_installed() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = S3Remote::new(&config, remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("AWS CLI (aws) is not installed"));
    }

    #[test]
    fn s3_uri_with_prefix() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    prefix: "esk/myapp"
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();

        struct DummyRunner;
        impl CommandRunner for DummyRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let remote = S3Remote::new(&config, remote_config, &DummyRunner);
        assert_eq!(
            remote.s3_uri("dev"),
            "s3://my-bucket/esk/myapp/secrets-dev.enc"
        );
        assert_eq!(
            remote.s3_uri("prod"),
            "s3://my-bucket/esk/myapp/secrets-prod.enc"
        );
    }

    #[test]
    fn s3_uri_without_prefix() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();

        struct DummyRunner;
        impl CommandRunner for DummyRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let remote = S3Remote::new(&config, remote_config, &DummyRunner);
        assert_eq!(remote.s3_uri("dev"), "s3://my-bucket/secrets-dev.enc");
    }

    #[test]
    fn s3_uri_encrypted_format() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: encrypted
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();

        struct DummyRunner;
        impl CommandRunner for DummyRunner {
            fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> Result<CommandOutput> {
                Ok(CommandOutput {
                    success: true,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                })
            }
        }

        let remote = S3Remote::new(&config, remote_config, &DummyRunner);
        assert_eq!(remote.s3_uri("dev"), "s3://my-bucket/secrets-dev.enc");
    }

    #[test]
    fn push_cleartext_sends_to_s3() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    prefix: backups
    format: cleartext
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"")]);
        let remote = S3Remote::new(&config, remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        let payload = StorePayload {
            secrets,
            version: 3,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1[0], "s3");
        assert_eq!(calls[0].1[1], "cp");
        assert_eq!(calls[0].1[2], "-");
        assert_eq!(calls[0].1[3], "s3://my-bucket/backups/secrets-dev.json");
    }

    #[test]
    fn push_skips_empty_env() {
        let yaml = r#"
project: myapp
environments: [dev, prod]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = S3Remote::new(&config, remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:prod".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();
        assert!(calls(&runner).is_empty());
    }

    #[test]
    fn pull_cleartext_parses_response() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();

        let payload = StorePayload {
            secrets: {
                let mut m = BTreeMap::new();
                m.insert("API_KEY".to_string(), "sk_test".to_string());
                m.insert("DB_URL".to_string(), "postgres://localhost".to_string());
                m
            },
            version: 7,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(json.as_bytes())]);
        let remote = S3Remote::new(&config, remote_config, &runner);

        let (secrets, version) = remote.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
    }

    #[test]
    fn pull_not_found_returns_none() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![fail_output(b"An error occurred (NoSuchKey)")]);
        let remote = S3Remote::new(&config, remote_config, &runner);

        assert!(remote.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn pull_auth_error_propagates() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![fail_output(b"Unable to locate credentials")]);
        let remote = S3Remote::new(&config, remote_config, &runner);

        let err = remote.pull(&config, "dev").unwrap_err();
        assert!(err.to_string().contains("Unable to locate credentials"));
    }

    #[test]
    fn base_args_includes_region_profile_endpoint() {
        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    region: us-west-2
    profile: myprofile
    endpoint: "https://r2.example.com"
    format: cleartext
"#;
        let config = make_config(yaml);
        let remote_config: S3RemoteConfig = config.remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"")]);
        let remote = S3Remote::new(&config, remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:dev".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
            env_last_changed_at: BTreeMap::new(),
        };

        remote.push(&payload, &config, "dev").unwrap();

        let calls = calls(&runner);
        let args = &calls[0].1;
        assert!(args.contains(&"--region".to_string()));
        assert!(args.contains(&"us-west-2".to_string()));
        assert!(args.contains(&"--profile".to_string()));
        assert!(args.contains(&"myprofile".to_string()));
        assert!(args.contains(&"--endpoint-url".to_string()));
        assert!(args.contains(&"https://r2.example.com".to_string()));
    }
}
