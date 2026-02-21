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
pub fn extract_env_secrets(
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn make_remote(secrets: &[(&str, &str)]) -> BTreeMap<String, String> {
        secrets
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn equal_versions_noop() {
        let local = make_payload(&[("KEY:dev", "val")], 5);
        let remote = make_remote(&[("KEY", "val")]);
        let result = reconcile(&local, &remote, 5, "dev");
        assert!(matches!(result.action, ReconcileAction::NoOp));
        assert!(result.pulled.is_empty());
        assert!(result.pushed.is_empty());
        assert!(result.merged_payload.is_none());
    }

    #[test]
    fn local_newer_push_local() {
        let local = make_payload(&[("KEY:dev", "new_val")], 5);
        let remote = make_remote(&[("KEY", "old_val")]);
        let result = reconcile(&local, &remote, 3, "dev");
        assert!(matches!(result.action, ReconcileAction::PushLocal));
        assert!(result.pulled.is_empty());
        assert_eq!(result.pushed, vec!["KEY"]);
    }

    #[test]
    fn remote_newer_pull_remote() {
        let local = make_payload(&[("KEY:dev", "old_val")], 3);
        let remote = make_remote(&[("KEY", "new_val")]);
        let result = reconcile(&local, &remote, 5, "dev");
        assert!(matches!(result.action, ReconcileAction::PullRemote));
        assert_eq!(result.pulled, vec!["KEY"]);
        assert!(result.pushed.is_empty());
    }

    #[test]
    fn pull_remote_merges_new_secrets() {
        let local = make_payload(&[("A:dev", "a_val")], 3);
        let remote = make_remote(&[("A", "a_val"), ("B", "b_val")]);
        let result = reconcile(&local, &remote, 5, "dev");
        assert!(matches!(result.action, ReconcileAction::PullRemote));
        assert_eq!(result.pulled, vec!["B"]);
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("B:dev").unwrap(), "b_val");
    }

    #[test]
    fn pull_remote_updates_existing() {
        let local = make_payload(&[("KEY:dev", "old")], 3);
        let remote = make_remote(&[("KEY", "new")]);
        let result = reconcile(&local, &remote, 5, "dev");
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "new");
    }

    #[test]
    fn pull_remote_local_only_pushed() {
        let local = make_payload(&[("A:dev", "a"), ("LOCAL:dev", "local_val")], 3);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 5, "dev");
        assert_eq!(result.pushed, vec!["LOCAL"]);
    }

    #[test]
    fn pull_remote_version_no_local_only() {
        let local = make_payload(&[("A:dev", "old")], 3);
        let remote = make_remote(&[("A", "new")]);
        let result = reconcile(&local, &remote, 5, "dev");
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.version, 5);
    }

    #[test]
    fn pull_remote_version_with_local_only() {
        let local = make_payload(&[("A:dev", "a"), ("LOCAL:dev", "x")], 3);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 5, "dev");
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.version, 6); // remote + 1
    }

    #[test]
    fn pull_remote_preserves_other_envs() {
        let local = make_payload(&[("KEY:dev", "dev_val"), ("KEY:prod", "prod_val")], 3);
        let remote = make_remote(&[("KEY", "new_dev")]);
        let result = reconcile(&local, &remote, 5, "dev");
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "new_dev");
        assert_eq!(merged.secrets.get("KEY:prod").unwrap(), "prod_val");
    }

    #[test]
    fn push_local_computes_diff() {
        let local = make_payload(&[("A:dev", "same"), ("B:dev", "changed")], 5);
        let remote = make_remote(&[("A", "same"), ("B", "old")]);
        let result = reconcile(&local, &remote, 3, "dev");
        assert_eq!(result.pushed, vec!["B"]);
    }

    #[test]
    fn push_local_same_values() {
        let local = make_payload(&[("A:dev", "val"), ("B:dev", "val2")], 5);
        let remote = make_remote(&[("A", "val"), ("B", "val2")]);
        let result = reconcile(&local, &remote, 3, "dev");
        assert!(result.pushed.is_empty());
    }

    #[test]
    fn noop_empty_stores() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[]);
        let result = reconcile(&local, &remote, 0, "dev");
        assert!(matches!(result.action, ReconcileAction::NoOp));
    }

    #[test]
    fn pull_remote_empty_local() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[("A", "a"), ("B", "b")]);
        let result = reconcile(&local, &remote, 5, "dev");
        assert!(matches!(result.action, ReconcileAction::PullRemote));
        assert_eq!(result.pulled.len(), 2);
    }

    #[test]
    fn push_local_empty_remote() {
        let local = make_payload(&[("A:dev", "a"), ("B:dev", "b")], 5);
        let remote = make_remote(&[]);
        let result = reconcile(&local, &remote, 3, "dev");
        assert!(matches!(result.action, ReconcileAction::PushLocal));
        assert_eq!(result.pushed.len(), 2);
    }

    #[test]
    fn extract_env_secrets_filters() {
        let mut secrets = BTreeMap::new();
        secrets.insert("A:dev".to_string(), "a".to_string());
        secrets.insert("B:prod".to_string(), "b".to_string());
        secrets.insert("C:dev".to_string(), "c".to_string());
        let result = extract_env_secrets(&secrets, "dev");
        assert_eq!(result.len(), 2);
        assert_eq!(result.get("A").unwrap(), "a");
        assert_eq!(result.get("C").unwrap(), "c");
    }

    #[test]
    fn extract_env_secrets_no_match() {
        let mut secrets = BTreeMap::new();
        secrets.insert("A:dev".to_string(), "a".to_string());
        let result = extract_env_secrets(&secrets, "staging");
        assert!(result.is_empty());
    }

    #[test]
    fn reconcile_v0_vs_v0() {
        let local = make_payload(&[("A:dev", "a")], 0);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 0, "dev");
        assert!(matches!(result.action, ReconcileAction::NoOp));
    }

    #[test]
    fn reconcile_v0_vs_v1() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 1, "dev");
        assert!(matches!(result.action, ReconcileAction::PullRemote));
        assert_eq!(result.pulled, vec!["A"]);
    }
}
