use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployIndex {
    pub records: BTreeMap<String, DeployRecord>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployRecord {
    pub target: String,
    pub value_hash: String,
    #[serde(alias = "last_synced_at")]
    pub last_deployed_at: String,
    #[serde(alias = "last_sync_status")]
    pub last_deploy_status: DeployStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeployStatus {
    Success,
    Failed,
}

impl DeployIndex {
    /// Sentinel hash for tombstone (deleted key) tracking.
    /// Never collides with real SHA-256 hashes (which are 64-char hex).
    pub const TOMBSTONE_HASH: &str = "__tombstone__";

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
                    "Warning: could not read deploy index ({}), starting fresh",
                    e
                );
                return Self::new(path);
            }
        };
        match serde_json::from_str::<DeployIndex>(&contents) {
            Ok(mut index) => {
                index.path = path.to_path_buf();
                index
            }
            Err(e) => {
                eprintln!("Warning: deploy index corrupted ({}), starting fresh", e);
                Self::new(path)
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(&self)?;
        let dir = self
            .path
            .parent()
            .context("deploy index path has no parent")?;
        let tmp = NamedTempFile::new_in(dir)?;
        std::fs::write(tmp.path(), json)?;
        tmp.persist(&self.path).with_context(|| {
            format!("failed to persist deploy index to {}", self.path.display())
        })?;
        Ok(())
    }

    /// Build a tracker key: "KEY:target:app:env" or "KEY:target:env"
    pub fn tracker_key(secret_key: &str, target: &str, app: Option<&str>, env: &str) -> String {
        match app {
            Some(a) => format!("{secret_key}:{target}:{a}:{env}"),
            None => format!("{secret_key}:{target}:{env}"),
        }
    }

    /// Determine if a deploy is needed.
    pub fn should_deploy(&self, tracker_key: &str, value_hash: &str, force: bool) -> bool {
        if force {
            return true;
        }
        match self.records.get(tracker_key) {
            None => true,
            Some(record) => {
                record.last_deploy_status == DeployStatus::Failed || record.value_hash != value_hash
            }
        }
    }

    pub fn record_success(&mut self, tracker_key: String, target: String, value_hash: String) {
        self.records.insert(
            tracker_key,
            DeployRecord {
                target,
                value_hash,
                last_deployed_at: chrono::Utc::now().to_rfc3339(),
                last_deploy_status: DeployStatus::Success,
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
            DeployRecord {
                target,
                value_hash,
                last_deployed_at: chrono::Utc::now().to_rfc3339(),
                last_deploy_status: DeployStatus::Failed,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_empty() {
        let index = DeployIndex::new(Path::new("/tmp/test.json"));
        assert!(index.records.is_empty());
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let index = DeployIndex::load(Path::new("/nonexistent/path/test.json"));
        assert!(index.records.is_empty());
    }

    #[test]
    fn load_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut index = DeployIndex::new(&path);
        index.record_success(
            "KEY:env:web:dev".to_string(),
            "env:web:dev".to_string(),
            "abc".to_string(),
        );
        index.save().unwrap();

        let loaded = DeployIndex::load(&path);
        assert_eq!(loaded.records.len(), 1);
        assert!(loaded.records.contains_key("KEY:env:web:dev"));
    }

    #[test]
    fn load_corrupted_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        std::fs::write(&path, "not valid json").unwrap();
        let index = DeployIndex::load(&path);
        assert!(index.records.is_empty());
    }

    #[test]
    fn save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let mut index = DeployIndex::new(&path);
        index.record_success(
            "A:env:web:dev".to_string(),
            "env:web:dev".to_string(),
            "hash1".to_string(),
        );
        index.record_failure(
            "B:cf:prod".to_string(),
            "cf:prod".to_string(),
            "hash2".to_string(),
            "err".to_string(),
        );
        index.save().unwrap();

        let loaded = DeployIndex::load(&path);
        assert_eq!(loaded.records.len(), 2);
        assert_eq!(
            loaded.records["A:env:web:dev"].last_deploy_status,
            DeployStatus::Success
        );
        assert_eq!(
            loaded.records["B:cf:prod"].last_deploy_status,
            DeployStatus::Failed
        );
    }

    #[test]
    fn save_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.json");
        let index = DeployIndex::new(&path);
        index.save().unwrap();
        assert!(path.is_file());
    }

    #[test]
    fn tracker_key_with_app() {
        let key = DeployIndex::tracker_key("SECRET", "env", Some("web"), "dev");
        assert_eq!(key, "SECRET:env:web:dev");
    }

    #[test]
    fn tracker_key_without_app() {
        let key = DeployIndex::tracker_key("SECRET", "cloudflare", None, "prod");
        assert_eq!(key, "SECRET:cloudflare:prod");
    }

    #[test]
    fn should_deploy_force_true() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success("K".to_string(), "t".to_string(), "hash".to_string());
        assert!(index.should_deploy("K", "hash", true));
    }

    #[test]
    fn should_deploy_no_record() {
        let index = DeployIndex::new(Path::new("/tmp/test.json"));
        assert!(index.should_deploy("K", "hash", false));
    }

    #[test]
    fn should_deploy_hash_match_success() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success("K".to_string(), "t".to_string(), "hash".to_string());
        assert!(!index.should_deploy("K", "hash", false));
    }

    #[test]
    fn should_deploy_hash_mismatch() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success("K".to_string(), "t".to_string(), "old_hash".to_string());
        assert!(index.should_deploy("K", "new_hash", false));
    }

    #[test]
    fn should_deploy_previous_failure() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_failure(
            "K".to_string(),
            "t".to_string(),
            "hash".to_string(),
            "err".to_string(),
        );
        assert!(index.should_deploy("K", "hash", false));
    }

    #[test]
    fn record_success_sets_fields() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success(
            "K".to_string(),
            "env:web:dev".to_string(),
            "abc".to_string(),
        );
        let record = &index.records["K"];
        assert_eq!(record.target, "env:web:dev");
        assert_eq!(record.value_hash, "abc");
        assert_eq!(record.last_deploy_status, DeployStatus::Success);
        assert!(record.last_error.is_none());
    }

    #[test]
    fn record_failure_sets_fields() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_failure(
            "K".to_string(),
            "cf:prod".to_string(),
            "abc".to_string(),
            "timeout".to_string(),
        );
        let record = &index.records["K"];
        assert_eq!(record.target, "cf:prod");
        assert_eq!(record.value_hash, "abc");
        assert_eq!(record.last_deploy_status, DeployStatus::Failed);
        assert_eq!(record.last_error.as_deref(), Some("timeout"));
    }

    #[test]
    fn record_overwrites_previous() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_failure(
            "K".to_string(),
            "t".to_string(),
            "h1".to_string(),
            "err".to_string(),
        );
        index.record_success("K".to_string(), "t".to_string(), "h2".to_string());
        let record = &index.records["K"];
        assert_eq!(record.last_deploy_status, DeployStatus::Success);
        assert_eq!(record.value_hash, "h2");
    }

    #[test]
    fn hash_value_deterministic() {
        let h1 = DeployIndex::hash_value("hello");
        let h2 = DeployIndex::hash_value("hello");
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_value_different_inputs() {
        let h1 = DeployIndex::hash_value("hello");
        let h2 = DeployIndex::hash_value("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_value_empty_string() {
        let hash = DeployIndex::hash_value("");
        // SHA-256 of empty string is well-known
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn tombstone_hash_is_not_valid_sha256() {
        // TOMBSTONE_HASH must never collide with a real SHA-256 output
        let any_hash = DeployIndex::hash_value("anything");
        assert_ne!(DeployIndex::TOMBSTONE_HASH, any_hash);
        assert_ne!(DeployIndex::TOMBSTONE_HASH.len(), 64); // SHA-256 hex is 64 chars
    }

    #[test]
    fn should_deploy_tombstone_success_skips() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success(
            "K".to_string(),
            "t".to_string(),
            DeployIndex::TOMBSTONE_HASH.to_string(),
        );
        assert!(!index.should_deploy("K", DeployIndex::TOMBSTONE_HASH, false));
    }

    #[test]
    fn should_deploy_tombstone_failure_retries() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_failure(
            "K".to_string(),
            "t".to_string(),
            DeployIndex::TOMBSTONE_HASH.to_string(),
            "err".to_string(),
        );
        assert!(index.should_deploy("K", DeployIndex::TOMBSTONE_HASH, false));
    }
}
