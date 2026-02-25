pub mod aws_secrets_manager;
pub mod azure_key_vault;
pub mod bitwarden;
pub mod cloud_file;
pub mod doppler;
pub mod gcp_secret_manager;
pub mod onepassword;
pub mod s3;
pub mod sops;
pub mod hashicorp_vault;

use anyhow::Result;
use std::collections::BTreeMap;

use crate::targets::CommandRunner;
use crate::config::Config;
use crate::store::{validate_key, StorePayload};

/// The key used to store version metadata in remote payloads.
pub const ESK_VERSION_KEY: &str = "_esk_version";

/// Extract bare-key secrets for a specific environment from a store payload.
/// Returns the filtered secrets (with `:env` suffix stripped) and the resolved version.
/// Returns `None` if no secrets match the given environment.
pub fn extract_env_secrets(
    payload: &StorePayload,
    env: &str,
) -> Option<(BTreeMap<String, String>, u64)> {
    let suffix = format!(":{env}");
    let env_secrets: BTreeMap<String, String> = payload
        .secrets
        .iter()
        .filter_map(|(k, v)| {
            k.strip_suffix(&suffix)
                .map(|bare| (bare.to_string(), v.clone()))
        })
        .collect();

    if env_secrets.is_empty() {
        return None;
    }

    let version = payload
        .env_versions
        .get(env)
        .copied()
        .unwrap_or(payload.version);

    Some((env_secrets, version))
}

/// Parse a pulled string-valued secret map back into composite-key secrets.
/// Extracts the version from `ESK_VERSION_KEY`, strips it from the map,
/// and re-adds the `:env` suffix to all remaining keys.
pub fn parse_pulled_secrets(
    data: BTreeMap<String, String>,
    env: &str,
) -> (BTreeMap<String, String>, u64) {
    let version: u64 = match data.get(ESK_VERSION_KEY) {
        Some(v) => match v.parse() {
            Ok(n) => n,
            Err(_) => {
                let _ = cliclack::log::warning(format!(
                    "Remote returned unparseable {ESK_VERSION_KEY}: '{v}'. Defaulting to version 0."
                ));
                0
            }
        },
        None => {
            let _ = cliclack::log::warning(format!(
                "Remote did not include {ESK_VERSION_KEY}. Defaulting to version 0."
            ));
            0
        }
    };

    let composite: BTreeMap<String, String> = data
        .into_iter()
        .filter(|(k, _)| k != ESK_VERSION_KEY)
        .filter(|(k, _)| {
            if validate_key(k).is_err() {
                let _ = cliclack::log::warning(format!("Skipping invalid key from remote: '{k}'"));
                false
            } else {
                true
            }
        })
        .map(|(k, v)| (format!("{k}:{env}"), v))
        .collect();

    (composite, version)
}

/// A sync remote that stores/retrieves the full secret state.
///
/// Unlike deploy targets, remotes
/// store or backup the entire secret list as a source of truth.
pub trait SyncRemote {
    fn name(&self) -> &str;

    /// Validate that external dependencies are available before push/pull.
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Push store state to this remote for a given environment.
    fn push(&self, payload: &StorePayload, config: &Config, env: &str) -> Result<()>;

    /// Pull store state from this remote for a given environment.
    /// Returns (composite_key_secrets, version), or None if nothing stored.
    fn pull(&self, config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>>;
}

/// Health status of a configured remote.
pub struct RemoteHealth {
    pub name: String,
    pub ok: bool,
    pub message: String,
}

struct RemoteCandidate<'a> {
    remote: Box<dyn SyncRemote + 'a>,
    ok_message: &'static str,
}

fn remote_candidates<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<RemoteCandidate<'a>> {
    let mut candidates: Vec<RemoteCandidate<'a>> = Vec::new();

    if let Some(op_config) = config.onepassword_remote_config() {
        candidates.push(RemoteCandidate {
            remote: Box::new(onepassword::OnePasswordRemote::new(
                config, op_config, runner,
            )),
            ok_message: "vault accessible",
        });
    }

    for (name, cf_config) in config.cloud_file_remote_configs() {
        candidates.push(RemoteCandidate {
            remote: Box::new(cloud_file::CloudFileRemote::new(
                name,
                config.project.clone(),
                cf_config,
            )),
            ok_message: "directory writable",
        });
    }

    if let Some(vault_config) = config.remote_config::<crate::config::HashicorpVaultRemoteConfig>("vault") {
        candidates.push(RemoteCandidate {
            remote: Box::new(hashicorp_vault::HashicorpVaultRemote::new(config, vault_config, runner)),
            ok_message: "authenticated",
        });
    }

    if let Some(bw_config) =
        config.remote_config::<crate::config::BitwardenRemoteConfig>("bitwarden")
    {
        candidates.push(RemoteCandidate {
            remote: Box::new(bitwarden::BitwardenRemote::new(config, bw_config, runner)),
            ok_message: "authenticated",
        });
    }

    if let Some(s3_config) = config.remote_config::<crate::config::S3RemoteConfig>("s3") {
        candidates.push(RemoteCandidate {
            remote: Box::new(s3::S3Remote::new(config, s3_config, runner)),
            ok_message: "CLI available",
        });
    }

    if let Some(gcp_config) = config.remote_config::<crate::config::GcpSecretManagerRemoteConfig>("gcp") {
        candidates.push(RemoteCandidate {
            remote: Box::new(gcp_secret_manager::GcpSecretManagerRemote::new(config, gcp_config, runner)),
            ok_message: "authenticated",
        });
    }

    if let Some(azure_config) = config.remote_config::<crate::config::AzureKeyVaultRemoteConfig>("azure") {
        candidates.push(RemoteCandidate {
            remote: Box::new(azure_key_vault::AzureKeyVaultRemote::new(config, azure_config, runner)),
            ok_message: "authenticated",
        });
    }

    if let Some(doppler_config) =
        config.remote_config::<crate::config::DopplerRemoteConfig>("doppler")
    {
        candidates.push(RemoteCandidate {
            remote: Box::new(doppler::DopplerRemote::new(config, doppler_config, runner)),
            ok_message: "authenticated",
        });
    }

    if let Some(sops_config) = config.remote_config::<crate::config::SopsRemoteConfig>("sops") {
        candidates.push(RemoteCandidate {
            remote: Box::new(sops::SopsRemote::new(config, sops_config, runner)),
            ok_message: "CLI available",
        });
    }

    if let Some(asm_config) =
        config.remote_config::<crate::config::AwsSecretsManagerRemoteConfig>("aws_secrets_manager")
    {
        candidates.push(RemoteCandidate {
            remote: Box::new(aws_secrets_manager::AwsSecretsManagerRemote::new(
                config, asm_config, runner,
            )),
            ok_message: "CLI available",
        });
    }

    candidates
}

fn needs_cli_secret_arg_warning(name: &str) -> bool {
    matches!(name, "1password" | "bitwarden")
}

/// Check the health of all configured remotes without filtering.
/// Returns one entry per configured remote with preflight pass/fail.
pub fn check_remote_health(config: &Config, runner: &dyn CommandRunner) -> Vec<RemoteHealth> {
    let mut health = Vec::new();
    for candidate in remote_candidates(config, runner) {
        let name = candidate.remote.name().to_string();
        match candidate.remote.preflight() {
            Ok(()) => health.push(RemoteHealth {
                name,
                ok: true,
                message: candidate.ok_message.to_string(),
            }),
            Err(e) => health.push(RemoteHealth {
                name,
                ok: false,
                message: e.to_string(),
            }),
        }
    }
    health
}

use std::sync::Once;

static OP_WARNING: Once = Once::new();

/// Build all configured remotes from the config.
/// Runs preflight checks and filters out remotes that fail, printing warnings.
pub fn build_remotes<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<Box<dyn SyncRemote + 'a>> {
    let mut remotes: Vec<Box<dyn SyncRemote + 'a>> = Vec::new();

    for candidate in remote_candidates(config, runner) {
        let remote = candidate.remote;
        match remote.preflight() {
            Ok(()) => {
                if needs_cli_secret_arg_warning(remote.name()) {
                    OP_WARNING.call_once(|| {
                        let _ = cliclack::log::warning(format!(
                            "{}: security note: secret values are passed as CLI args and may be visible in local process listings\n",
                            remote.name()
                        ));
                    });
                }
                remotes.push(remote);
            }
            Err(e) => {
                let _ = cliclack::log::warning(format!("Skipping {} remote: {}", remote.name(), e));
            }
        }
    }

    remotes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{CommandOpts, CommandOutput};
    use crate::test_support::ErrorCommandRunner;

    struct DummyRunner;
    impl CommandRunner for DummyRunner {
        fn run(&self, _program: &str, _args: &[&str], _opts: CommandOpts) -> Result<CommandOutput> {
            Ok(CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn parse_pulled_secrets_filters_invalid_keys() {
        let mut data = BTreeMap::new();
        data.insert("VALID_KEY".to_string(), "val1".to_string());
        data.insert("invalid-key".to_string(), "val2".to_string());
        data.insert("also invalid!".to_string(), "val3".to_string());
        data.insert("ANOTHER_VALID".to_string(), "val4".to_string());
        data.insert(ESK_VERSION_KEY.to_string(), "5".to_string());

        let (composite, version) = parse_pulled_secrets(data, "dev");
        assert_eq!(version, 5);
        assert_eq!(composite.len(), 2);
        assert!(composite.contains_key("VALID_KEY:dev"));
        assert!(composite.contains_key("ANOTHER_VALID:dev"));
        assert!(!composite.contains_key("invalid-key:dev"));
        assert!(!composite.contains_key("also invalid!:dev"));
    }

    #[test]
    fn parse_pulled_secrets_all_invalid_keys() {
        let mut data = BTreeMap::new();
        data.insert("bad-key".to_string(), "val".to_string());
        data.insert(ESK_VERSION_KEY.to_string(), "1".to_string());

        let (composite, version) = parse_pulled_secrets(data, "dev");
        assert_eq!(version, 1);
        assert!(composite.is_empty());
    }

    #[test]
    fn build_remotes_empty_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, "project: x\nenvironments: [dev]").unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let built_remotes = build_remotes(&config, &runner);
        assert!(built_remotes.is_empty());
    }

    #[test]
    fn build_remotes_with_onepassword() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let built_remotes = build_remotes(&config, &runner);
        assert_eq!(built_remotes.len(), 1);
        assert_eq!(built_remotes[0].name(), "1password");
    }

    #[test]
    fn build_remotes_with_cloud_file() {
        let dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: x
environments: [dev]
remotes:
  dropbox:
    type: cloud_file
    path: {}
    format: encrypted
"#,
            cloud_dir.path().display()
        );
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let built_remotes = build_remotes(&config, &runner);
        assert_eq!(built_remotes.len(), 1);
        assert_eq!(built_remotes[0].name(), "dropbox");
    }

    #[test]
    fn build_remotes_filters_failing_preflight() {
        let dir = tempfile::tempdir().unwrap();
        // onepassword will fail (runner fails), cloud_file with existing dir will pass
        let cloud_dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
  testcloud:
    type: cloud_file
    path: {}
    format: cleartext
"#,
            cloud_dir.path().display()
        );
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let runner = ErrorCommandRunner::new("not found");
        let built_remotes = build_remotes(&config, &runner);
        // onepassword fails preflight, cloud_file with existing dir passes
        assert_eq!(built_remotes.len(), 1);
        assert_eq!(built_remotes[0].name(), "testcloud");
    }

    #[test]
    fn build_remotes_filters_cloud_file_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
  testcloud:
    type: cloud_file
    path: /nonexistent/path/nowhere
    format: cleartext
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let built_remotes = build_remotes(&config, &runner);
        assert!(built_remotes.is_empty());
    }

    #[test]
    fn check_remote_health_op_ok() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let health = check_remote_health(&config, &DummyRunner);
        assert_eq!(health.len(), 1);
        assert!(health[0].ok);
        assert_eq!(health[0].name, "1password");
    }

    #[test]
    fn check_remote_health_op_fails() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let runner = ErrorCommandRunner::new("op not found");
        let health = check_remote_health(&config, &runner);
        assert_eq!(health.len(), 1);
        assert!(!health[0].ok);
        assert!(health[0].message.contains("op) is not installed"));
    }

    #[test]
    fn check_remote_health_no_remotes() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = "project: x\nenvironments: [dev]";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let health = check_remote_health(&config, &DummyRunner);
        assert!(health.is_empty());
    }

    #[test]
    fn build_remotes_mixed() {
        let dir = tempfile::tempdir().unwrap();
        let cloud_dir1 = tempfile::tempdir().unwrap();
        let cloud_dir2 = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test
  dropbox:
    type: cloud_file
    path: {}
    format: encrypted
  gdrive:
    type: cloud_file
    path: {}
    format: cleartext
"#,
            cloud_dir1.path().display(),
            cloud_dir2.path().display()
        );
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let built_remotes = build_remotes(&config, &runner);
        assert_eq!(built_remotes.len(), 3);
    }
}
