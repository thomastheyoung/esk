use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncIndex {
    pub records: BTreeMap<String, SyncRecord>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncRecord {
    pub target: String,
    pub value_hash: String,
    pub last_synced_at: String,
    pub last_sync_status: SyncStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SyncStatus {
    Success,
    Failed,
}

impl SyncIndex {
    pub fn new(path: &Path) -> Self {
        Self {
            records: BTreeMap::new(),
            path: path.to_path_buf(),
        }
    }

    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::new(path));
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut index: SyncIndex = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        index.path = path.to_path_buf();
        Ok(index)
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self)?;
        let dir = self.path.parent().context("sync index path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), json)?;
        tmp.persist(&self.path)
            .with_context(|| format!("failed to persist sync index to {}", self.path.display()))?;
        Ok(())
    }

    /// Build a tracker key: "KEY:adapter:app:env" or "KEY:adapter:env"
    pub fn tracker_key(secret_key: &str, adapter: &str, app: Option<&str>, env: &str) -> String {
        match app {
            Some(a) => format!("{secret_key}:{adapter}:{a}:{env}"),
            None => format!("{secret_key}:{adapter}:{env}"),
        }
    }

    /// Determine if a sync is needed.
    pub fn should_sync(&self, tracker_key: &str, value_hash: &str, force: bool) -> bool {
        if force {
            return true;
        }
        match self.records.get(tracker_key) {
            None => true,
            Some(record) => {
                record.last_sync_status == SyncStatus::Failed || record.value_hash != value_hash
            }
        }
    }

    pub fn record_success(&mut self, tracker_key: String, target: String, value_hash: String) {
        self.records.insert(
            tracker_key,
            SyncRecord {
                target,
                value_hash,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
                last_sync_status: SyncStatus::Success,
                last_error: None,
            },
        );
    }

    pub fn record_failure(
        &mut self,
        tracker_key: String,
        target: String,
        value_hash: String,
        error: String,
    ) {
        self.records.insert(
            tracker_key,
            SyncRecord {
                target,
                value_hash,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
                last_sync_status: SyncStatus::Failed,
                last_error: Some(error),
            },
        );
    }

    /// Compute SHA-256 hash of a value.
    pub fn hash_value(value: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(value.as_bytes());
        hex::encode(hasher.finalize())
    }
}
