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
use crate::store::{decrypt_with_key, derive_key, encrypt_with_key, SecretStore, StorePayload};
use crate::targets::{CommandOpts, CommandRunner};

const S3_SYNC_DOMAIN: &[u8] = b"esk-s3-sync-v1";

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
}

impl SyncRemote for S3Remote<'_> {
    fn name(&self) -> &'static str {
        "s3"
    }

    fn uses_cleartext_format(&self) -> bool {
        matches!(
            self.remote_config.format,
            crate::config::CloudFileFormat::Cleartext
        )
    }

    fn preflight(&self) -> Result<()> {
        crate::targets::check_command(self.runner, "aws").map_err(|_| {
            anyhow::anyhow!(
                "AWS CLI (aws) is not installed or not in PATH. Install it from: https://aws.amazon.com/cli/"
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
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("AWS authentication failed: {stderr}");
        }

        Ok(())
    }

    fn push(&self, payload: &StorePayload, _config: &Config, env: &str) -> Result<()> {
        let env_payload = payload.for_env(env);

        if env_payload.secrets.is_empty() {
            return Ok(());
        }

        let s3_uri = self.s3_uri(env);
        let base = self.base_args();

        let content = match self.remote_config.format {
            CloudFileFormat::Encrypted => {
                let store = SecretStore::open(&self.config.root)?;
                let dk = derive_key(store.master_key(), S3_SYNC_DOMAIN);
                let json = serde_json::to_string(&env_payload)
                    .context("failed to serialize env payload")?;
                encrypt_with_key(&dk, &json)?
            }
            CloudFileFormat::Cleartext => serde_json::to_string_pretty(&env_payload)
                .context("failed to serialize env payload")?,
        };

        let mut args: Vec<&str> = vec!["s3", "cp", "-", &s3_uri];
        args.extend(base.iter().map(String::as_str));

        let output = self
            .runner
            .run(
                "aws",
                &args,
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
        let base = self.base_args();

        let mut args: Vec<&str> = vec!["s3", "cp", &s3_uri, "-"];
        args.extend(base.iter().map(String::as_str));

        let output = self
            .runner
            .run("aws", &args, CommandOpts::default())
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

        let payload: StorePayload = match self.remote_config.format {
            CloudFileFormat::Encrypted => {
                let store = SecretStore::open(&self.config.root)?;
                let dk = derive_key(store.master_key(), S3_SYNC_DOMAIN);
                let json = decrypt_with_key(&dk, content)?;
                serde_json::from_str(&json).context("failed to parse decrypted JSON from S3")?
            }
            CloudFileFormat::Cleartext => {
                serde_json::from_str(content).context("failed to parse secrets JSON from S3")?
            }
        };

        Ok(Some((
            StorePayload::bare_to_composite(&payload.secrets, env),
            payload.version,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::CommandOutput;
    use crate::test_support::{ConfigFixture, ErrorCommandRunner, MockCommandRunner};




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
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"aws-cli/2.13.0"),
            ok_output(b"{\"Account\": \"123456789012\"}"),
        ]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].args, vec!["--version"]);
        assert_eq!(calls[1].args, vec!["sts", "get-caller-identity"]);
    }

    #[test]
    fn preflight_success_with_profile_region() {
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
    region: us-west-2
    profile: myprofile
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"aws-cli/2.13.0"),
            ok_output(b"{\"Account\": \"123456789012\"}"),
        ]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);
        assert!(remote.preflight().is_ok());
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        let sts_args = &calls[1].args;
        assert!(sts_args.contains(&"sts".to_string()));
        assert!(sts_args.contains(&"get-caller-identity".to_string()));
        assert!(sts_args.contains(&"--region".to_string()));
        assert!(sts_args.contains(&"us-west-2".to_string()));
        assert!(sts_args.contains(&"--profile".to_string()));
        assert!(sts_args.contains(&"myprofile".to_string()));
    }

    #[test]
    fn preflight_auth_failure() {
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![
            ok_output(b"aws-cli/2.13.0"),
            fail_output(b"Unable to locate credentials"),
        ]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("AWS authentication failed"));
    }

    #[test]
    fn preflight_aws_not_installed() {
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-secrets-bucket
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner = ErrorCommandRunner::missing_command();
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);
        let err = remote.preflight().unwrap_err();
        assert!(err.to_string().contains("AWS CLI (aws) is not installed"));
    }

    #[test]
    fn s3_uri_with_prefix() {
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

        let yaml = r#"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    prefix: "esk/myapp"
"#;
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();

        let remote = S3Remote::new(fixture.config(), remote_config, &DummyRunner);
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

        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();

        let remote = S3Remote::new(fixture.config(), remote_config, &DummyRunner);
        assert_eq!(remote.s3_uri("dev"), "s3://my-bucket/secrets-dev.enc");
    }

    #[test]
    fn s3_uri_encrypted_format() {
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

        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: encrypted
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();

        let remote = S3Remote::new(fixture.config(), remote_config, &DummyRunner);
        assert_eq!(remote.s3_uri("dev"), "s3://my-bucket/secrets-dev.enc");
    }

    #[test]
    fn push_cleartext_sends_to_s3() {
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    prefix: backups
    format: cleartext
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"")]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("API_KEY:dev".to_string(), "sk_test".to_string());
        let payload = StorePayload {
            secrets,
            version: 3,
            ..Default::default()
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].args[0], "s3");
        assert_eq!(calls[0].args[1], "cp");
        assert_eq!(calls[0].args[2], "-");
        assert_eq!(calls[0].args[3], "s3://my-bucket/backups/secrets-dev.json");
    }

    #[test]
    fn push_skips_empty_env() {
        let yaml = r"
project: myapp
environments: [dev, prod]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:prod".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            ..Default::default()
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn pull_cleartext_parses_response() {
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();

        let payload = StorePayload {
            secrets: {
                let mut m = BTreeMap::new();
                m.insert("API_KEY".to_string(), "sk_test".to_string());
                m.insert("DB_URL".to_string(), "postgres://localhost".to_string());
                m
            },
            version: 7,
            ..Default::default()
        };
        let json = serde_json::to_string(&payload).unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(json.as_bytes())]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);

        let (secrets, version) = remote.pull(fixture.config(), "dev").unwrap().unwrap();
        assert_eq!(version, 7);
        assert_eq!(secrets.get("API_KEY:dev").unwrap(), "sk_test");
        assert_eq!(secrets.get("DB_URL:dev").unwrap(), "postgres://localhost");
    }

    #[test]
    fn pull_not_found_returns_none() {
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![fail_output(b"An error occurred (NoSuchKey)")]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);

        assert!(remote.pull(fixture.config(), "dev").unwrap().is_none());
    }

    #[test]
    fn pull_auth_error_propagates() {
        let yaml = r"
project: myapp
environments: [dev]
remotes:
  s3:
    bucket: my-bucket
    format: cleartext
";
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner =
            MockCommandRunner::from_outputs(vec![fail_output(b"Unable to locate credentials")]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);

        let err = remote.pull(fixture.config(), "dev").unwrap_err();
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
        let fixture = ConfigFixture::new(yaml).expect("fixture");
        let remote_config: S3RemoteConfig = fixture.config().remote_config("s3").unwrap();
        let runner = MockCommandRunner::from_outputs(vec![ok_output(b"")]);
        let remote = S3Remote::new(fixture.config(), remote_config, &runner);

        let mut secrets = BTreeMap::new();
        secrets.insert("KEY:dev".to_string(), "val".to_string());
        let payload = StorePayload {
            secrets,
            version: 1,
            ..Default::default()
        };

        remote.push(&payload, fixture.config(), "dev").unwrap();

        let calls = runner.calls();
        let args = &calls[0].args;
        assert!(args.contains(&"--region".to_string()));
        assert!(args.contains(&"us-west-2".to_string()));
        assert!(args.contains(&"--profile".to_string()));
        assert!(args.contains(&"myprofile".to_string()));
        assert!(args.contains(&"--endpoint-url".to_string()));
        assert!(args.contains(&"https://r2.example.com".to_string()));
    }
}
