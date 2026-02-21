use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

use crate::config::{CloudFileFormat, CloudFilePluginConfig, Config};
use crate::store::{SecretStore, StorePayload};

use super::StoragePlugin;

pub struct CloudFilePlugin {
    name: String,
    plugin_config: CloudFilePluginConfig,
}

impl CloudFilePlugin {
    pub fn new(name: String, plugin_config: CloudFilePluginConfig) -> Self {
        Self {
            name,
            plugin_config,
        }
    }

    /// Expand tilde in path to home directory.
    fn expand_path(&self) -> Result<PathBuf> {
        let path = &self.plugin_config.path;
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

    fn push(&self, payload: &StorePayload, config: &Config, _env: &str) -> Result<()> {
        let base_path = self.expand_path()?;

        match self.plugin_config.format {
            CloudFileFormat::Encrypted => {
                // Copy .secrets.enc to cloud path
                let source = config.root.join(".secrets.enc");
                let dest = base_path.join("secrets.enc");
                let content = std::fs::read(&source)
                    .with_context(|| format!("failed to read {}", source.display()))?;
                Self::atomic_write(&dest, &content)?;
            }
            CloudFileFormat::Cleartext => {
                // Write JSON payload
                let dest = base_path.join("secrets.json");
                let json =
                    serde_json::to_string_pretty(payload).context("failed to serialize payload")?;
                Self::atomic_write(&dest, json.as_bytes())?;
            }
        }

        Ok(())
    }

    fn pull(&self, config: &Config, _env: &str) -> Result<Option<(BTreeMap<String, String>, u64)>> {
        let base_path = self.expand_path()?;

        match self.plugin_config.format {
            CloudFileFormat::Encrypted => {
                let source = base_path.join("secrets.enc");
                if !source.is_file() {
                    return Ok(None);
                }
                let content = std::fs::read_to_string(&source)
                    .with_context(|| format!("failed to read {}", source.display()))?;
                let content = content.trim();
                if content.is_empty() {
                    return Ok(None);
                }
                // Decrypt using local key
                let store = SecretStore::open(&config.root)?;
                let payload = store.decrypt_raw(content)?;
                Ok(Some((payload.secrets, payload.version)))
            }
            CloudFileFormat::Cleartext => {
                let source = base_path.join("secrets.json");
                if !source.is_file() {
                    return Ok(None);
                }
                let content = std::fs::read_to_string(&source)
                    .with_context(|| format!("failed to read {}", source.display()))?;
                let payload: StorePayload =
                    serde_json::from_str(&content).context("failed to parse secrets.json")?;
                Ok(Some((payload.secrets, payload.version)))
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
        }
    }

    #[test]
    fn cloud_file_preflight_success() {
        let cloud_dir = tempfile::tempdir().unwrap();
        let plugin = CloudFilePlugin::new(
            "dropbox".to_string(),
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
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let payload = make_payload(&[("KEY:dev", "val1"), ("KEY:prod", "val2")], 5);
        plugin.push(&payload, &config, "dev").unwrap();

        assert!(cloud_dir.path().join("secrets.json").is_file());

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 5);
        assert_eq!(secrets.get("KEY:dev").unwrap(), "val1");
        assert_eq!(secrets.get("KEY:prod").unwrap(), "val2");
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
            CloudFilePluginConfig {
                path: cloud_dir.path().to_string_lossy().to_string(),
                format: CloudFileFormat::Encrypted,
            },
        );

        let payload = store.payload().unwrap();
        plugin.push(&payload, &config, "dev").unwrap();

        assert!(cloud_dir.path().join("secrets.enc").is_file());

        let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
        assert_eq!(version, 1);
        assert_eq!(secrets.get("KEY:dev").unwrap(), "encrypted_val");
    }

    #[test]
    fn pull_nonexistent_returns_none() {
        let project_dir = tempfile::tempdir().unwrap();
        let cloud_dir = tempfile::tempdir().unwrap();
        let config = make_config_with_store(project_dir.path());

        let plugin = CloudFilePlugin::new(
            "test".to_string(),
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
            CloudFilePluginConfig {
                path: nested.to_string_lossy().to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let payload = make_payload(&[("A:dev", "1")], 1);
        plugin.push(&payload, &config, "dev").unwrap();
        assert!(nested.join("secrets.json").is_file());
    }

    #[test]
    fn tilde_expansion() {
        let plugin = CloudFilePlugin::new(
            "test".to_string(),
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
            CloudFilePluginConfig {
                path: "/absolute/path".to_string(),
                format: CloudFileFormat::Cleartext,
            },
        );

        let expanded = plugin.expand_path().unwrap();
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }
}
