use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use crate::config::{CloudFileFormat, CloudFilePluginConfig, Config};
use crate::reconcile::extract_env_secrets;
use crate::store::{SecretStore, StorePayload};

use super::StoragePlugin;

pub struct CloudFilePlugin {
    name: String,
    project: String,
    plugin_config: CloudFilePluginConfig,
}

impl CloudFilePlugin {
    pub fn new(name: String, project: String, plugin_config: CloudFilePluginConfig) -> Self {
        Self {
            name,
            project,
            plugin_config,
        }
    }

    /// Expand `{project}` and tilde in path.
    fn expand_path(&self) -> Result<PathBuf> {
        let path = self.plugin_config.path.replace("{project}", &self.project);
        if let Some(rest) = path.strip_prefix("~/") {
            let home = std::env::var("HOME").context("HOME environment variable not set")?;
            Ok(PathBuf::from(home).join(rest))
        } else {
            Ok(PathBuf::from(path))
        }
    }

    /// Atomic write: write to temp file then rename.
    fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
        let dir = path.parent().context("path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), content)?;
        tmp.persist(path)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Build a per-env StorePayload with bare keys for a given environment.
    /// Uses env-specific version when available, falling back to global version.
    fn env_payload(payload: &StorePayload, env: &str) -> StorePayload {
        let bare = extract_env_secrets(&payload.secrets, env);
        let version = payload
            .env_versions
            .get(env)
            .copied()
            .unwrap_or(payload.version);
        StorePayload {
            secrets: bare,
            version,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
        }
    }

    /// Convert bare keys from a per-env file back to composite keys.
    fn bare_to_composite(
        bare: &BTreeMap<String, String>,
        env: &str,
    ) -> BTreeMap<String, String> {
        bare.iter()
            .map(|(k, v)| (format!("{k}:{env}"), v.clone()))
            .collect()
    }

    /// Remove the legacy global file if per-env files are being written.
    fn cleanup_legacy_file(&self, base_path: &Path) -> Result<()> {
        let legacy = match self.plugin_config.format {
            CloudFileFormat::Encrypted => base_path.join("secrets.enc"),
            CloudFileFormat::Cleartext => base_path.join("secrets.json"),
        };
        if legacy.is_file() {
            std::fs::remove_file(&legacy)
                .with_context(|| format!("failed to remove legacy file {}", legacy.display()))?;
        }
        Ok(())
    }
}

impl StoragePlugin for CloudFilePlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn preflight(&self) -> Result<()> {
        let path = self.expand_path()?;
        if !path.is_dir() {
            anyhow::bail!(
                "{} sync folder not found at {}. Make sure the cloud sync app is installed and the folder exists.",
                self.name,
                path.display()
            );
        }
        Ok(())
    }

    fn push(&self, payload: &StorePayload, config: &Config, env: &str) -> Result<()> {
        let base_path = self.expand_path()?;
        let env_payload = Self::env_payload(payload, env);

        match self.plugin_config.format {
            CloudFileFormat::Encrypted => {
                // Build per-env payload, encrypt it using the local store key
                let store = SecretStore::open(&config.root)?;
                let json = serde_json::to_string(&env_payload)
                    .context("failed to serialize env payload")?;
                let encrypted = store.encrypt_raw(&json)?;
                let dest = base_path.join(format!("secrets-{env}.enc"));
                Self::atomic_write(&dest, encrypted.as_bytes())?;
            }
            CloudFileFormat::Cleartext => {
                let dest = base_path.join(format!("secrets-{env}.json"));
                let json = serde_json::to_string_pretty(&env_payload)
                    .context("failed to serialize env payload")?;
                Self::atomic_write(&dest, json.as_bytes())?;
            }
        }

        // One-time migration: remove legacy global file
        self.cleanup_legacy_file(&base_path)?;

        Ok(())
    }

    fn pull(&self, config: &Config, env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let base_path = self.expand_path()?;

        match self.plugin_config.format {
            CloudFileFormat::Encrypted => {
                let per_env = base_path.join(format!("secrets-{env}.enc"));
                let source = if per_env.is_file() {
                    per_env
                } else {
                    // Backward compat: fall back to legacy global file
                    let legacy = base_path.join("secrets.enc");
                    if !legacy.is_file() {
                        return Ok(None);
                    }
                    eprintln!(
                        "Warning: reading legacy secrets.enc for {env}. Run `lockbox push --env {env}` to migrate to per-env files."
                    );
                    legacy
                };
                let content = std::fs::read_to_string(&source)
                    .with_context(|| format!("failed to read {}", source.display()))?;
                let content = content.trim();
                if content.is_empty() {
                    return Ok(None);
                }
                let store = SecretStore::open(&config.root)?;
                let payload = store.decrypt_raw(content)?;
                // Per-env files have bare keys — convert to composite
                // Legacy files have composite keys — detect by checking if keys contain ":"
                let has_composite = payload.secrets.keys().any(|k| k.contains(':'));
                if has_composite {
                    // Legacy global file — return as-is
                    Ok(Some((payload.secrets, payload.version)))
                } else {
                    // Per-env file — convert bare keys to composite
                    Ok(Some((
                        Self::bare_to_composite(&payload.secrets, env),
                        payload.version,
                    )))
                }
            }
            CloudFileFormat::Cleartext => {
                let per_env = base_path.join(format!("secrets-{env}.json"));
                let source = if per_env.is_file() {
                    per_env
                } else {
                    let legacy = base_path.join("secrets.json");
                    if !legacy.is_file() {
                        return Ok(None);
                    }
                    eprintln!(
                        "Warning: reading legacy secrets.json for {env}. Run `lockbox push --env {env}` to migrate to per-env files."
                    );
                    legacy
                };
                let content = std::fs::read_to_string(&source)
                    .with_context(|| format!("failed to read {}", source.display()))?;
                let payload: StorePayload =
                    serde_json::from_str(&content).context("failed to parse secrets JSON")?;
                let has_composite = payload.secrets.keys().any(|k| k.contains(':'));
                if has_composite {
                    Ok(Some((payload.secrets, payload.version)))
                } else {
                    Ok(Some((
                        Self::bare_to_composite(&payload.secrets, env),
                        payload.version,
                    )))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SecretStore;

    fn make_config_with_store(dir: &Path) -> Config {
        let yaml = "project: testapp\nenvironments: [dev, prod]";
        let path = dir.join("lockbox.yaml");
        std::fs::write(&path, yaml).unwrap();
        SecretStore::load_or_create(dir).unwrap();
        Config::load(&path).unwrap()
    }

    fn make_payload(secrets: &[(&str, &str)], version: u64) -> StorePayload {
        let mut map = BTreeMap::new();
        for (k, v) in secrets {
            map.insert(k.to_string(), v.to_string());
        }
        StorePayload {
            secrets: map,
            version,
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
        }
    }

    #[test]
    fn cloud_file_preflight_success() {
        let cloud_dir = tempfile::tempdir().unwrap();
        let plugin = CloudFilePlugin::new(
            "dropbox".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );
        assert!(plugin.preflight().is_ok());
    }

    #[test]
    fn cloud_file_preflight_missing_dir() {
        let plugin = CloudFilePlugin::new(
            "dropbox".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: "/nonexistent/path/that/does/not/exist".to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );
        let err = plugin.preflight().unwrap_err();
        assert!(err.to_string().contains("dropbox sync folder not found"));
        assert!(err.to_string().contains("/nonexistent/path/that/does/not/exist"));
    }

    #[test]
    fn cleartext_push_pull_roundtrip() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        let plugin = CloudFilePlugin::new(
            "test_cloud".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let payload = make_payload(&[("KEY:dev", "val1"), ("KEY:prod", "val2")], 5);
        plugin.push(&payload, &config, "dev").unwrap();

        // Per-env file created, not global
        assert!(cloud_dir.path().join("secrets-dev.json").is_file());
        assert!(!cloud_dir.path().join("secrets.json").is_file());

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 5);
        assert_eq!(secrets.get("KEY:dev").unwrap(), "val1");
        // prod key should NOT be in the dev-specific file
        assert!(!secrets.contains_key("KEY:prod"));
    }

    #[test]
    fn encrypted_push_pull_roundtrip() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        // Write some secrets to the store
        let store = SecretStore::open(&config.root).unwrap();
        store.set("KEY", "dev", "encrypted_val").unwrap();

        let plugin = CloudFilePlugin::new(
            "test_enc".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Encrypted,
            },
        );

        let payload = store.payload().unwrap();
        plugin.push(&payload, &config, "dev").unwrap();

        // Per-env file created, not global
        assert!(cloud_dir.path().join("secrets-dev.enc").is_file());
        assert!(!cloud_dir.path().join("secrets.enc").is_file());

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 1);
        assert_eq!(secrets.get("KEY:dev").unwrap(), "encrypted_val");
    }

    #[test]
    fn per_env_isolation() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        // Push dev secrets
        let payload = make_payload(&[("KEY:dev", "dev_val"), ("KEY:prod", "prod_val")], 5);
        plugin.push(&payload, &config, "dev").unwrap();
        // Push prod secrets
        plugin.push(&payload, &config, "prod").unwrap();

        // Dev file should only have dev secrets
        let (dev_secrets, _) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(dev_secrets.get("KEY:dev").unwrap(), "dev_val");
        assert!(!dev_secrets.contains_key("KEY:prod"));

        // Prod file should only have prod secrets
        let (prod_secrets, _) = plugin.pull(&config, "prod").unwrap().unwrap();
        assert_eq!(prod_secrets.get("KEY:prod").unwrap(), "prod_val");
        assert!(!prod_secrets.contains_key("KEY:dev"));
    }

    #[test]
    fn backward_compat_legacy_cleartext() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        // Write a legacy global secrets.json with composite keys
        let legacy_payload = make_payload(&[("KEY:dev", "legacy_val")], 3);
        let json = serde_json::to_string_pretty(&legacy_payload).unwrap();
        std::fs::write(cloud_dir.path().join("secrets.json"), json).unwrap();

        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        // Pull should fall back to legacy file
        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 3);
        assert_eq!(secrets.get("KEY:dev").unwrap(), "legacy_val");
    }

    #[test]
    fn legacy_cleanup_on_push() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        // Create a legacy file
        std::fs::write(cloud_dir.path().join("secrets.json"), "{}").unwrap();
        assert!(cloud_dir.path().join("secrets.json").is_file());

        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let payload = make_payload(&[("KEY:dev", "val")], 1);
        plugin.push(&payload, &config, "dev").unwrap();

        // Legacy file removed, per-env file created
        assert!(!cloud_dir.path().join("secrets.json").is_file());
        assert!(cloud_dir.path().join("secrets-dev.json").is_file());
    }

    #[test]
    fn pull_nonexistent_returns_none() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        assert!(plugin.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn pull_encrypted_nonexistent_returns_none() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Encrypted,
            },
        );

        assert!(plugin.pull(&config, "dev").unwrap().is_none());
    }

    #[test]
    fn push_creates_parent_dirs() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let nested = cloud_dir.path().join("deep/nested/path");
        let config = make_config_with_store(project_dir.path());

        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: nested.to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let payload = make_payload(&[("A:dev", "1")], 1);
        plugin.push(&payload, &config, "dev").unwrap();
        assert!(nested.join("secrets-dev.json").is_file());
    }

    #[test]
    fn tilde_expansion() {
        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: "~/test/path".to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let expanded = plugin.expand_path().unwrap();
        assert!(!expanded.to_string_lossy().contains('~'));
        assert!(expanded.to_string_lossy().ends_with("/test/path"));
    }

    #[test]
    fn no_tilde_expansion_for_absolute() {
        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "testapp".to_string(),
            CloudFilePluginConfig {
                path: "/absolute/path".to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let expanded = plugin.expand_path().unwrap();
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn project_interpolation() {
        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "myapp".to_string(),
            CloudFilePluginConfig {
                path: "/cloud/lockbox/{project}".to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let expanded = plugin.expand_path().unwrap();
        assert_eq!(expanded, PathBuf::from("/cloud/lockbox/myapp"));
    }

    #[test]
    fn project_interpolation_with_tilde() {
        let plugin = CloudFilePlugin::new(
            "test".to_string(),
            "myapp".to_string(),
            CloudFilePluginConfig {
                path: "~/Dropbox/lockbox/{project}".to_string(),
                format: CloudFileFormat::Encrypted,
            },
        );

        let expanded = plugin.expand_path().unwrap();
        assert!(!expanded.to_string_lossy().contains('~'));
        assert!(!expanded.to_string_lossy().contains("{project}"));
        assert!(expanded.to_string_lossy().ends_with("/Dropbox/lockbox/myapp"));
    }
}
