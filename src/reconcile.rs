use std::collections::BTreeMap;

use crate::store::StorePayload;

#[derive(Debug)]
pub enum ReconcileAction {
    /// Remote is newer — pull remote secrets into local
    PullRemote,
    /// Local is newer — push local diffs to remote
    PushLocal,
    /// Versions are equal — no action needed
    NoOp,
}

#[derive(Debug)]
pub struct ReconcileResult {
    pub action: ReconcileAction,
    /// Secrets pulled from remote into local store
    pub pulled: Vec<String>,
    /// Secrets that should be pushed to remote
    pub pushed: Vec<String>,
    /// The merged payload (if action is PullRemote)
    pub merged_payload: Option<StorePayload>,
}

/// Reconcile local store with remote (1Password) data for a given environment.
///
/// - Remote newer: merge remote secrets into local, any local-only secrets get pushed back
/// - Local newer: compute diff of secrets to push
/// - Equal: no-op
pub fn reconcile(
    local: &StorePayload,
    remote_secrets: &BTreeMap<String, String>,
    remote_version: u64,
    env: &str,
) -> ReconcileResult {
    let local_env_secrets = extract_env_secrets(&local.secrets, env);

    match local.version.cmp(&remote_version) {
        std::cmp::Ordering::Less => {
            // Remote is newer — pull remote, push local-only keys back
            let mut merged = local.secrets.clone();
            let mut pulled = Vec::new();
            let mut pushed = Vec::new();

            // Pull all remote secrets into local
            for (key, value) in remote_secrets {
                let composite = format!("{key}:{env}");
                let local_val = merged.get(&composite);
                if local_val.map(|v| v.as_str()) != Some(value.as_str()) {
                    merged.insert(composite, value.clone());
                    pulled.push(key.clone());
                }
            }

            // Any local-only keys (not in remote) should be pushed
            for key in local_env_secrets.keys() {
                if !remote_secrets.contains_key(key) {
                    pushed.push(key.clone());
                }
            }

            // Use remote version, +1 if we have local-only keys to push back
            let merged_version = if pushed.is_empty() {
                remote_version
            } else {
                remote_version + 1
            };

            let merged_payload = StorePayload {
                secrets: merged,
                version: merged_version,
            };

            ReconcileResult {
                action: ReconcileAction::PullRemote,
                pulled,
                pushed,
                merged_payload: Some(merged_payload),
            }
        }
        std::cmp::Ordering::Greater => {
            // Local is newer — compute what to push
            let mut pushed = Vec::new();

            for (key, value) in &local_env_secrets {
                let remote_val = remote_secrets.get(key);
                if remote_val.map(|v| v.as_str()) != Some(value.as_str()) {
                    pushed.push(key.clone());
                }
            }

            ReconcileResult {
                action: ReconcileAction::PushLocal,
                pulled: Vec::new(),
                pushed,
                merged_payload: None,
            }
        }
        std::cmp::Ordering::Equal => ReconcileResult {
            action: ReconcileAction::NoOp,
            pulled: Vec::new(),
            pushed: Vec::new(),
            merged_payload: None,
        },
    }
}

/// Extract secrets for a given environment from the composite-keyed map.
/// Returns a map of bare keys to values.
fn extract_env_secrets(
    secrets: &BTreeMap<String, String>,
    env: &str,
) -> BTreeMap<String, String> {
    let suffix = format!(":{env}");
    secrets
        .iter()
        .filter_map(|(k, v)| {
            k.strip_suffix(&suffix)
                .map(|bare| (bare.to_string(), v.clone()))
        })
        .collect()
}
