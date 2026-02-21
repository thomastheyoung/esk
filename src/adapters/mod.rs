pub mod cloudflare;
pub mod convex;
pub mod env_file;
pub mod onepass;

use anyhow::Result;
use crate::config::ResolvedTarget;

pub struct SyncResult {
    pub key: String,
    #[allow(dead_code)]
    pub target: ResolvedTarget,
    pub success: bool,
    pub error: Option<String>,
}

/// Secret with its key and value, ready for syncing.
pub struct SecretValue {
    pub key: String,
    pub value: String,
    pub vendor: String,
}

pub trait SyncAdapter {
    #[allow(dead_code)]
    fn name(&self) -> &str;

    /// Sync a single secret to a target.
    fn sync_secret(&self, key: &str, value: &str, target: &ResolvedTarget) -> Result<()>;

    /// Sync a batch of secrets. Default implementation loops sync_secret.
    fn sync_batch(&self, secrets: &[SecretValue], target: &ResolvedTarget) -> Vec<SyncResult> {
        secrets
            .iter()
            .map(|s| match self.sync_secret(&s.key, &s.value, target) {
                Ok(()) => SyncResult {
                    key: s.key.clone(),
                    target: target.clone(),
                    success: true,
                    error: None,
                },
                Err(e) => SyncResult {
                    key: s.key.clone(),
                    target: target.clone(),
                    success: false,
                    error: Some(e.to_string()),
                },
            })
            .collect()
    }
}
