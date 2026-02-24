use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginIndex {
    pub records: BTreeMap<String, PluginRecord>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
    pub plugin: String,
    pub environment: String,
    pub pushed_version: u64,
    pub last_pushed_at: String,
    pub last_push_status: PushStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PushStatus {
    Success,
    Failed,
}

impl PluginIndex {
    pub fn new(path: &Path) -> Self {
        Self {
            records: BTreeMap::new(),
            path: path.to_path_buf(),
        }
    }

    pub fn load(path: &Path) -> Self {
        if !path.is_file() {
            return Self::new(path);
        }
        let contents = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "Warning: could not read plugin index ({}), starting fresh",
                    e
                );
                return Self::new(path);
            }
        };
        match serde_json::from_str::<PluginIndex>(&contents) {
            Ok(mut index) => {
                index.path = path.to_path_buf();
                index
            }
            Err(e) => {
                eprintln!("Warning: plugin index corrupted ({}), starting fresh", e);
                Self::new(path)
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self)?;
        let dir = self
            .path
            .parent()
            .context("plugin index path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), json)?;
        tmp.persist(&self.path).with_context(|| {
            format!("failed to persist plugin index to {}", self.path.display())
        })?;
        Ok(())
    }

    /// Build a tracker key: "plugin:env"
    pub fn tracker_key(plugin: &str, env: &str) -> String {
        format!("{plugin}:{env}")
    }

    pub fn record_success(&mut self, plugin: &str, env: &str, version: u64) {
        let key = Self::tracker_key(plugin, env);
        self.records.insert(
            key,
            PluginRecord {
                plugin: plugin.to_string(),
                environment: env.to_string(),
                pushed_version: version,
                last_pushed_at: chrono::Utc::now().to_rfc3339(),
                last_push_status: PushStatus::Success,
                last_error: None,
            },
        );
    }

    pub fn record_failure(&mut self, plugin: &str, env: &str, version: u64, error: String) {
        let key = Self::tracker_key(plugin, env);
        self.records.insert(
            key,
            PluginRecord {
                plugin: plugin.to_string(),
                environment: env.to_string(),
                pushed_version: version,
                last_pushed_at: chrono::Utc::now().to_rfc3339(),
                last_push_status: PushStatus::Failed,
                last_error: Some(error),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_empty() {
        let index = PluginIndex::new(Path::new("/tmp/test.json"));
        assert!(index.records.is_empty());
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let index = PluginIndex::load(Path::new("/nonexistent/path/test.json"));
        assert!(index.records.is_empty());
    }

    #[test]
    fn load_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut index = PluginIndex::new(&path);
        index.record_success("1password", "dev", 3);
        index.save().unwrap();

        let loaded = PluginIndex::load(&path);
        assert_eq!(loaded.records.len(), 1);
        assert!(loaded.records.contains_key("1password:dev"));
    }

    #[test]
    fn load_corrupted_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        std::fs::write(&path, "not valid json").unwrap();
        let index = PluginIndex::load(&path);
        assert!(index.records.is_empty());
    }

    #[test]
    fn save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut index = PluginIndex::new(&path);
        index.record_success("1password", "dev", 5);
        index.record_failure("dropbox", "prod", 3, "timeout".to_string());
        index.save().unwrap();

        let loaded = PluginIndex::load(&path);
        assert_eq!(loaded.records.len(), 2);
        assert_eq!(
            loaded.records["1password:dev"].last_push_status,
            PushStatus::Success
        );
        assert_eq!(
            loaded.records["dropbox:prod"].last_push_status,
            PushStatus::Failed
        );
    }

    #[test]
    fn save_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let index = PluginIndex::new(&path);
        index.save().unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn tracker_key_format() {
        assert_eq!(
            PluginIndex::tracker_key("1password", "dev"),
            "1password:dev"
        );
        assert_eq!(PluginIndex::tracker_key("dropbox", "prod"), "dropbox:prod");
    }

    #[test]
    fn record_success_sets_fields() {
        let mut index = PluginIndex::new(Path::new("/tmp/test.json"));
        index.record_success("1password", "dev", 5);
        let record = &index.records["1password:dev"];
        assert_eq!(record.plugin, "1password");
        assert_eq!(record.environment, "dev");
        assert_eq!(record.pushed_version, 5);
        assert_eq!(record.last_push_status, PushStatus::Success);
        assert!(record.last_error.is_none());
    }

    #[test]
    fn record_failure_sets_fields() {
        let mut index = PluginIndex::new(Path::new("/tmp/test.json"));
        index.record_failure("dropbox", "prod", 3, "timeout".to_string());
        let record = &index.records["dropbox:prod"];
        assert_eq!(record.plugin, "dropbox");
        assert_eq!(record.environment, "prod");
        assert_eq!(record.pushed_version, 3);
        assert_eq!(record.last_push_status, PushStatus::Failed);
        assert_eq!(record.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn record_overwrites_previous() {
        let mut index = PluginIndex::new(Path::new("/tmp/test.json"));
        index.record_failure("1password", "dev", 3, "err".to_string());
        index.record_success("1password", "dev", 5);
        let record = &index.records["1password:dev"];
        assert_eq!(record.last_push_status, PushStatus::Success);
        assert_eq!(record.pushed_version, 5);
    }
}
