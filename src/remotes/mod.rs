pub mod aws_secrets_manager;
pub mod azure_key_vault;
pub mod bitwarden;
pub mod cloud_file;
pub mod doppler;
pub mod gcp_secret_manager;
pub mod hashicorp_vault;
pub mod infisical;
pub mod onepassword;
pub mod s3;
pub mod sops;

use anyhow::{Context, Result};
use std::collections::BTreeMap;

use crate::config::Config;
use crate::store::{validate_key, StorePayload};
use crate::targets::{CommandRunner, PreflightItem};

/// The key used to store version metadata in remote payloads.
pub const ESK_VERSION_KEY: &str = crate::store::REMOTE_VERSION_KEY;
/// Flat-provider metadata key containing a JSON object of key -> env version.
pub const ESK_TOMBSTONES_KEY: &str = crate::store::REMOTE_TOMBSTONES_KEY;

#[derive(Clone)]
pub struct RemoteSnapshot {
    pub secrets: BTreeMap<String, String>,
    pub tombstones: BTreeMap<String, u64>,
    pub version: u64,
}

impl std::fmt::Debug for RemoteSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSnapshot")
            .field("secrets", &format_args!("<{} entries>", self.secrets.len()))
            .field("tombstones", &self.tombstones)
            .field("version", &self.version)
            .finish()
    }
}

impl RemoteSnapshot {
    pub fn from_payload(payload: StorePayload, env: &str) -> Result<Self> {
        let secrets = StorePayload::bare_to_composite(&payload.secrets, env);
        let tombstones = payload
            .tombstones
            .into_iter()
            .map(|(key, version)| (format!("{key}:{env}"), version))
            .collect();
        let snapshot = Self {
            secrets,
            tombstones,
            version: payload.version,
        };
        snapshot.validate(env)?;
        Ok(snapshot)
    }

    pub fn validate(&self, env: &str) -> Result<()> {
        let suffix = format!(":{env}");
        for key in self.secrets.keys() {
            let bare = key
                .strip_suffix(&suffix)
                .context("remote snapshot contains a key outside the requested environment")?;
            validate_key(bare).context("remote snapshot contains an invalid key")?;
            if bare == ESK_VERSION_KEY || bare == ESK_TOMBSTONES_KEY {
                anyhow::bail!("remote snapshot contains a reserved key");
            }
            if self.tombstones.contains_key(key) {
                anyhow::bail!("remote snapshot contains overlapping value and tombstone keys");
            }
        }
        for (key, version) in &self.tombstones {
            let bare = key
                .strip_suffix(&suffix)
                .context("remote tombstone is outside the requested environment")?;
            validate_key(bare).context("remote tombstone contains an invalid key")?;
            if bare == ESK_VERSION_KEY || bare == ESK_TOMBSTONES_KEY {
                anyhow::bail!("remote tombstone contains a reserved key");
            }
            if *version > self.version {
                anyhow::bail!("remote tombstone version exceeds snapshot version");
            }
        }
        Ok(())
    }
}

pub fn flat_snapshot_map(
    payload: &StorePayload,
    env: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    let env_payload = payload.for_env(env);
    if env_payload.secrets.is_empty() && env_payload.tombstones.is_empty() {
        return Ok(None);
    }
    if env_payload.secrets.contains_key(ESK_VERSION_KEY)
        || env_payload.secrets.contains_key(ESK_TOMBSTONES_KEY)
    {
        anyhow::bail!("secret key collides with reserved sync metadata");
    }
    let mut data = env_payload.secrets;
    data.insert(ESK_VERSION_KEY.to_string(), env_payload.version.to_string());
    if !env_payload.tombstones.is_empty() {
        data.insert(
            ESK_TOMBSTONES_KEY.to_string(),
            serde_json::to_string(&env_payload.tombstones)
                .context("failed to serialize deletion metadata")?,
        );
    }
    Ok(Some(data))
}

pub fn parse_flat_snapshot(
    mut data: BTreeMap<String, String>,
    env: &str,
) -> Result<RemoteSnapshot> {
    let tombstone_metadata = data.remove(ESK_TOMBSTONES_KEY);
    let version = match data.remove(ESK_VERSION_KEY) {
        Some(value) => value
            .parse::<u64>()
            .context("remote snapshot has invalid version metadata")?,
        None if tombstone_metadata.is_some() => {
            anyhow::bail!("remote tombstones require version metadata")
        }
        None => 0,
    };
    let tombstones: BTreeMap<String, u64> = match tombstone_metadata {
        Some(metadata) => {
            serde_json::from_str(&metadata).context("remote tombstone metadata is malformed")?
        }
        None => BTreeMap::new(),
    };
    RemoteSnapshot::from_payload(
        StorePayload {
            secrets: data,
            tombstones,
            version,
            ..Default::default()
        },
        env,
    )
}

/// Parse a pulled string-valued secret map back into composite-key secrets.
/// Extracts the version from `ESK_VERSION_KEY`, strips it from the map,
/// and re-adds the `:env` suffix to all remaining keys.
pub fn parse_pulled_secrets(
    data: BTreeMap<String, String>,
    env: &str,
) -> (BTreeMap<String, String>, u64) {
    let version: u64 = if let Some(v) = data.get(ESK_VERSION_KEY) {
        if let Ok(n) = v.parse() {
            n
        } else {
            let _ = cliclack::log::warning(format!(
                "Remote returned unparseable {ESK_VERSION_KEY}: '{v}'. Defaulting to version 0."
            ));
            0
        }
    } else {
        let _ = cliclack::log::warning(format!(
            "Remote did not include {ESK_VERSION_KEY}. Defaulting to version 0."
        ));
        0
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
pub trait SyncRemote: Send + Sync {
    fn name(&self) -> &str;

    /// Validate that external dependencies are available before push/pull.
    fn preflight(&self) -> Result<()> {
        Ok(())
    }

    /// Push store state to this remote for a given environment.
    fn push(&self, payload: &StorePayload, config: &Config, env: &str) -> Result<()>;

    /// Pull store state from this remote for a given environment.
    /// Returns (composite_key_secrets, version), or None if nothing stored.
    fn pull(&self, config: &Config, env: &str) -> Result<Option<RemoteSnapshot>>;

    /// Whether this remote passes secret values as CLI arguments (visible in `ps`).
    fn passes_value_as_cli_arg(&self) -> bool {
        false
    }

    /// Whether this remote stores secrets in cleartext format.
    fn uses_cleartext_format(&self) -> bool {
        false
    }
}

/// Health status of a configured remote.
pub struct RemoteHealth {
    pub name: String,
    pub status: crate::targets::HealthStatus,
}

pub(crate) struct RemoteCandidate<'a> {
    pub(crate) remote: Box<dyn SyncRemote + 'a>,
    pub(crate) ok_message: &'static str,
}

impl PreflightItem for RemoteCandidate<'_> {
    fn preflight_name(&self) -> &str {
        self.remote.name()
    }

    fn preflight(&self) -> Result<()> {
        self.remote.preflight()
    }

    fn ok_message(&self) -> &str {
        self.ok_message
    }
}

fn remote_candidates<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<RemoteCandidate<'a>> {
    use crate::config::TypedRemoteConfig;

    config
        .typed_remotes
        .iter()
        .map(|typed| {
            let ok_message = typed.ok_message();
            let remote: Box<dyn SyncRemote + 'a> = match typed {
                TypedRemoteConfig::OnePassword(cfg) => Box::new(
                    onepassword::OnePasswordRemote::new(config, cfg.clone(), runner),
                ),
                TypedRemoteConfig::CloudFile { name, config: cfg } => {
                    Box::new(cloud_file::CloudFileRemote::new(
                        name.clone(),
                        config.project.clone(),
                        cfg.clone(),
                    ))
                }
                TypedRemoteConfig::AwsSecretsManager(cfg) => Box::new(
                    aws_secrets_manager::AwsSecretsManagerRemote::new(config, cfg.clone(), runner),
                ),
                TypedRemoteConfig::Bitwarden(cfg) => {
                    Box::new(bitwarden::BitwardenRemote::new(config, cfg.clone(), runner))
                }
                TypedRemoteConfig::Vault(cfg) => Box::new(
                    hashicorp_vault::HashicorpVaultRemote::new(config, cfg.clone(), runner),
                ),
                TypedRemoteConfig::S3(cfg) => {
                    Box::new(s3::S3Remote::new(config, cfg.clone(), runner))
                }
                TypedRemoteConfig::Gcp(cfg) => Box::new(
                    gcp_secret_manager::GcpSecretManagerRemote::new(config, cfg.clone(), runner),
                ),
                TypedRemoteConfig::Azure(cfg) => Box::new(
                    azure_key_vault::AzureKeyVaultRemote::new(config, cfg.clone(), runner),
                ),
                TypedRemoteConfig::Doppler(cfg) => {
                    Box::new(doppler::DopplerRemote::new(cfg.clone(), runner))
                }
                TypedRemoteConfig::Infisical(cfg) => {
                    Box::new(infisical::InfisicalRemote::new(cfg.clone(), runner))
                }
                TypedRemoteConfig::Sops(cfg) => {
                    Box::new(sops::SopsRemote::new(config, cfg.clone(), runner))
                }
            };
            RemoteCandidate { remote, ok_message }
        })
        .collect()
}

/// Render remote health with animated spinners, returning health results.
///
/// Creates candidates from config, runs `run_preflight_section()` with the
/// given section name, and converts results to `Vec<RemoteHealth>`.
pub fn render_remote_health(
    config: &Config,
    runner: &dyn CommandRunner,
    section_name: &str,
) -> Vec<RemoteHealth> {
    let candidates = remote_candidates(config, runner);
    let items: Vec<&dyn PreflightItem> =
        candidates.iter().map(|c| c as &dyn PreflightItem).collect();
    let results = crate::targets::run_preflight_section(&items, section_name);
    candidates
        .iter()
        .zip(results)
        .map(|(c, (ok, msg))| RemoteHealth {
            name: c.remote.name().to_string(),
            status: if ok {
                crate::targets::HealthStatus::Ok(msg)
            } else {
                crate::targets::HealthStatus::Failed(msg)
            },
        })
        .collect()
}

/// Check the health of all configured remotes without rendering a human report.
/// Returns one entry per configured remote with preflight pass/fail and runs
/// all preflight checks in parallel.
/// The preflight section still uses stderr for progress, leaving stdout safe for
/// machine-readable callers such as `esk doctor --json`.
pub fn check_remote_health(config: &Config, runner: &dyn CommandRunner) -> Vec<RemoteHealth> {
    let candidates = remote_candidates(config, runner);
    if candidates.is_empty() {
        return Vec::new();
    }

    let items: Vec<&dyn PreflightItem> =
        candidates.iter().map(|c| c as &dyn PreflightItem).collect();
    let results = crate::targets::run_preflight_section(&items, "Remotes");
    candidates
        .iter()
        .zip(results)
        .map(|(c, (ok, msg))| RemoteHealth {
            name: c.remote.name().to_string(),
            status: if ok {
                crate::targets::HealthStatus::Ok(msg)
            } else {
                crate::targets::HealthStatus::Failed(msg)
            },
        })
        .collect()
}

/// Build all configured remotes from the config.
/// Runs preflight checks in parallel and filters out remotes that fail, printing warnings.
pub fn build_remotes<'a>(
    config: &'a Config,
    runner: &'a dyn CommandRunner,
) -> Vec<Box<dyn SyncRemote + 'a>> {
    let candidates = remote_candidates(config, runner);
    if candidates.is_empty() {
        return Vec::new();
    }

    let items: Vec<&dyn PreflightItem> =
        candidates.iter().map(|c| c as &dyn PreflightItem).collect();
    let results = crate::targets::run_preflight_section(&items, "Remotes");

    // Emit security warnings for passing remotes
    let mut security_warnings: Vec<String> = Vec::new();
    for (i, (ok, _)) in results.iter().enumerate() {
        if *ok {
            let remote = &candidates[i].remote;
            if remote.passes_value_as_cli_arg() {
                security_warnings.push(format!(
                    "{}: secret values are passed as CLI args and may be visible in local process listings",
                    remote.name()
                ));
            }
            if remote.uses_cleartext_format() {
                security_warnings.push(format!(
                    "{}: secrets are stored in cleartext — set `format: encrypted` to protect them at rest",
                    remote.name()
                ));
            }
        }
    }

    for warning in &security_warnings {
        let _ = cliclack::log::warning(warning);
    }

    // Filter to passing remotes
    candidates
        .into_iter()
        .zip(results)
        .filter_map(|(c, (ok, _))| if ok { Some(c.remote) } else { None })
        .collect()
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
    fn flat_snapshot_roundtrips_tombstones_without_secret_leakage() {
        let payload = StorePayload {
            secrets: BTreeMap::from([("LIVE:dev".to_string(), "value".to_string())]),
            version: 7,
            tombstones: BTreeMap::from([("DELETED:dev".to_string(), 7)]),
            env_versions: BTreeMap::from([("dev".to_string(), 7)]),
            ..Default::default()
        };
        let encoded = flat_snapshot_map(&payload, "dev").unwrap().unwrap();
        let snapshot = parse_flat_snapshot(encoded, "dev").unwrap();

        assert_eq!(snapshot.version, 7);
        assert_eq!(snapshot.secrets.len(), 1);
        assert_eq!(snapshot.secrets.get("LIVE:dev"), Some(&"value".to_string()));
        assert_eq!(snapshot.tombstones.get("DELETED:dev"), Some(&7));
        assert!(!snapshot.secrets.contains_key("_esk_tombstones:dev"));
    }

    #[test]
    fn flat_snapshot_rejects_malformed_or_unversioned_tombstones() {
        let malformed = BTreeMap::from([
            (ESK_VERSION_KEY.to_string(), "2".to_string()),
            (ESK_TOMBSTONES_KEY.to_string(), "not-json".to_string()),
        ]);
        assert!(parse_flat_snapshot(malformed, "dev").is_err());

        let unversioned =
            BTreeMap::from([(ESK_TOMBSTONES_KEY.to_string(), r#"{"KEY":1}"#.to_string())]);
        assert!(parse_flat_snapshot(unversioned, "dev").is_err());
    }

    #[test]
    fn remote_snapshot_debug_redacts_values() {
        let snapshot = RemoteSnapshot {
            secrets: BTreeMap::from([("KEY:dev".to_string(), "sensitive-value".to_string())]),
            tombstones: BTreeMap::new(),
            version: 1,
        };
        let debug = format!("{snapshot:?}");
        assert!(!debug.contains("sensitive-value"));
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
        let yaml = r"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test-{environment}
";
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
            r"
project: x
environments: [dev]
remotes:
  dropbox:
    type: cloud_file
    path: {}
    format: encrypted
",
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
            r"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test-{{environment}}
  testcloud:
    type: cloud_file
    path: {}
    format: cleartext
",
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
    fn build_remotes_creates_cloud_file_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cloud_dir = dir.path().join("new-cloud-dir");
        let yaml = format!(
            r"
project: x
environments: [dev]
remotes:
  testcloud:
    type: cloud_file
    path: {}
    format: cleartext
",
            cloud_dir.display()
        );
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();
        let runner = DummyRunner;
        let built_remotes = build_remotes(&config, &runner);
        assert_eq!(built_remotes.len(), 1);
        assert!(cloud_dir.is_dir());
    }

    #[test]
    fn check_remote_health_op_ok() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test-{environment}
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let health = check_remote_health(&config, &DummyRunner);
        assert_eq!(health.len(), 1);
        assert!(health[0].status.is_ok());
        assert_eq!(health[0].name, "1password");
    }

    #[test]
    fn check_remote_health_op_fails() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test-{environment}
";
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        let config = Config::load(&path).unwrap();

        let runner = ErrorCommandRunner::new("op not found");
        let health = check_remote_health(&config, &runner);
        assert_eq!(health.len(), 1);
        assert!(!health[0].status.is_ok());
        assert!(health[0].status.message().contains("Install from:"));
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
            r"
project: x
environments: [dev]
remotes:
  1password:
    vault: V
    item_pattern: test-{{environment}}
  dropbox:
    type: cloud_file
    path: {}
    format: encrypted
  gdrive:
    type: cloud_file
    path: {}
    format: cleartext
",
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
