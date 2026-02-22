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
///
/// Tombstone-aware: if local has a tombstone for a key at version T and remote
/// has the key, the tombstone wins if T > remote_version, otherwise remote wins.
pub fn reconcile(
    local: &StorePayload,
    remote_secrets: &BTreeMap<String, String>,
    remote_version: u64,
    env: &str,
) -> ReconcileResult {
    let local_env_secrets = extract_env_secrets(&local.secrets, env);

    // Use env-specific version when available, fall back to global
    let local_version = local
        .env_versions
        .get(env)
        .copied()
        .unwrap_or(local.version);

    match local_version.cmp(&remote_version) {
        std::cmp::Ordering::Less => {
            // Remote is newer — pull remote, push local-only keys back
            let mut merged = local.secrets.clone();
            let mut merged_tombstones = local.tombstones.clone();
            let mut pulled = Vec::new();
            let mut pushed = Vec::new();

            // Pull all remote secrets into local
            for (key, value) in remote_secrets {
                let composite = format!("{key}:{env}");
                // Check if local has a tombstone for this key
                if let Some(&tomb_version) = local.tombstones.get(&composite) {
                    if tomb_version > remote_version {
                        // Tombstone wins — key stays deleted
                        continue;
                    }
                    // Remote is newer than tombstone — resurrect
                    merged_tombstones.remove(&composite);
                }
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

            let mut merged_env_versions = local.env_versions.clone();
            merged_env_versions.insert(env.to_string(), merged_version);

            let merged_payload = StorePayload {
                secrets: merged,
                version: merged_version,
                tombstones: merged_tombstones,
                env_versions: merged_env_versions,
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

/// Result of multi-source reconciliation.
#[derive(Debug)]
pub struct MultiReconcileResult {
    /// The merged payload to write locally.
    pub merged_payload: StorePayload,
    /// Names of sources that need updating (their version was behind the merged result).
    pub sources_to_update: Vec<String>,
    /// Whether anything changed compared to local.
    pub local_changed: bool,
}

/// Reconcile local store against N remote sources.
///
/// Finds the highest version, starts with that as base, merges in
/// unique secrets from other sources. Sets version to max+1 if merges occurred.
///
/// Tombstone-aware: a tombstone at version T beats a value at version < T.
/// A value at version > T beats a tombstone at version T.
pub fn reconcile_multi(
    local: &StorePayload,
    remotes: &[(&str, &BTreeMap<String, String>, u64)], // (name, composite_secrets, version)
    env: Option<&str>,
) -> MultiReconcileResult {
    // Use env-specific local version when available
    let local_version = env
        .and_then(|e| local.env_versions.get(e).copied())
        .unwrap_or(local.version);

    // Find the max version across local and all remotes
    let mut max_version = local_version;
    for (_, _, version) in remotes {
        if *version > max_version {
            max_version = *version;
        }
    }

    // Start with local secrets as the base
    let mut merged = local.secrets.clone();
    let mut merged_tombstones = local.tombstones.clone();
    let mut had_merge = false;
    let mut sources_to_update = Vec::new();

    // If local is behind, pull in secrets from the highest-version remote first
    if local_version < max_version {
        for (_name, secrets, version) in remotes {
            if *version == max_version {
                for (key, value) in *secrets {
                    // Check tombstones: if local deleted this key at a higher version, skip
                    if let Some(&tomb_version) = local.tombstones.get(key) {
                        if tomb_version > *version {
                            continue; // tombstone wins
                        }
                        // Remote is newer — resurrect
                        merged_tombstones.remove(key);
                    }
                    let existing = merged.get(key);
                    if existing.map(|v| v.as_str()) != Some(value.as_str()) {
                        merged.insert(key.clone(), value.clone());
                        had_merge = true;
                    }
                }
            }
        }
    }

    // Merge unique secrets from lower-version sources
    for (_name, secrets, version) in remotes {
        if *version < max_version {
            for (key, value) in *secrets {
                if !merged.contains_key(key) {
                    // Check tombstones
                    if let Some(&tomb_version) = merged_tombstones.get(key) {
                        if tomb_version > *version {
                            continue; // tombstone wins
                        }
                        merged_tombstones.remove(key);
                    }
                    merged.insert(key.clone(), value.clone());
                    had_merge = true;
                }
            }
        }
    }

    let final_version = if had_merge {
        max_version + 1
    } else {
        max_version
    };

    // Determine which sources need updating
    for (name, _, version) in remotes {
        if *version < final_version {
            sources_to_update.push(name.to_string());
        }
    }

    let local_changed = local_version != final_version || merged != local.secrets;

    let mut merged_env_versions = local.env_versions.clone();
    if let Some(e) = env {
        merged_env_versions.insert(e.to_string(), final_version);
    }

    MultiReconcileResult {
        merged_payload: StorePayload {
            secrets: merged,
            version: final_version,
            tombstones: merged_tombstones,
            env_versions: merged_env_versions,
        },
        sources_to_update,
        local_changed,
    }
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
            tombstones: BTreeMap::new(),
            env_versions: BTreeMap::new(),
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

    fn make_payload_with_tombstones(
        secrets: &[(&str, &str)],
        tombstones: &[(&str, u64)],
        version: u64,
    ) -> StorePayload {
        let mut map = BTreeMap::new();
        for (k, v) in secrets {
            map.insert(k.to_string(), v.to_string());
        }
        let mut tomb_map = BTreeMap::new();
        for (k, v) in tombstones {
            tomb_map.insert(k.to_string(), *v);
        }
        StorePayload {
            secrets: map,
            version,
            tombstones: tomb_map,
            env_versions: BTreeMap::new(),
        }
    }

    #[test]
    fn tombstone_wins_when_newer_than_remote() {
        // Local deleted KEY at v4, remote has KEY at v3
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 4)], 4);
        let remote = make_remote(&[("KEY", "old_val")]);
        let result = reconcile(&local, &remote, 3, "dev");
        // Local is newer: PushLocal, KEY stays deleted
        assert!(matches!(result.action, ReconcileAction::PushLocal));
    }

    #[test]
    fn tombstone_loses_when_remote_newer() {
        // Local deleted KEY at v3, remote has KEY at v5
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 3)], 3);
        let remote = make_remote(&[("KEY", "new_val")]);
        let result = reconcile(&local, &remote, 5, "dev");
        assert!(matches!(result.action, ReconcileAction::PullRemote));
        let merged = result.merged_payload.unwrap();
        // Remote value wins — key is resurrected
        assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "new_val");
        // Tombstone should be removed
        assert!(!merged.tombstones.contains_key("KEY:dev"));
    }

    #[test]
    fn tombstone_preserved_on_pull_when_newer() {
        // Local deleted KEY at v4, remote is newer (v5) but has KEY at lower tomb version
        // Local tombstone at v4 < remote_version v5, so remote resurrects
        let local = make_payload_with_tombstones(&[("OTHER:dev", "x")], &[("KEY:dev", 4)], 4);
        let remote = make_remote(&[("KEY", "resurrected"), ("OTHER", "x")]);
        let result = reconcile(&local, &remote, 5, "dev");
        assert!(matches!(result.action, ReconcileAction::PullRemote));
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "resurrected");
    }

    #[test]
    fn multi_tombstone_wins_over_lower_version_remote() {
        // Local deleted KEY:dev at v4
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 4)], 4);
        let remote = make_composite(&[("KEY:dev", "old")]);
        // Remote at v3 — tombstone at v4 should win
        let result = reconcile_multi(&local, &[("op", &remote, 3)], None);
        assert!(!result.merged_payload.secrets.contains_key("KEY:dev"));
        assert!(result.merged_payload.tombstones.contains_key("KEY:dev"));
    }

    #[test]
    fn multi_tombstone_loses_to_higher_version_remote() {
        // Local deleted KEY:dev at v3
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 3)], 3);
        let remote = make_composite(&[("KEY:dev", "new")]);
        // Remote at v5 — should resurrect
        let result = reconcile_multi(&local, &[("op", &remote, 5)], None);
        assert_eq!(
            result.merged_payload.secrets.get("KEY:dev").unwrap(),
            "new"
        );
        assert!(!result.merged_payload.tombstones.contains_key("KEY:dev"));
    }

    #[test]
    fn reconcile_uses_env_version() {
        // Local global version is 5, but env-specific for dev is 3
        let mut local = make_payload(&[("KEY:dev", "old")], 5);
        local.env_versions.insert("dev".to_string(), 3);
        let remote = make_remote(&[("KEY", "new")]);
        // Remote at v4 — with env version 3 local is behind
        let result = reconcile(&local, &remote, 4, "dev");
        assert!(matches!(result.action, ReconcileAction::PullRemote));
    }

    #[test]
    fn reconcile_env_version_local_newer() {
        let mut local = make_payload(&[("KEY:dev", "new")], 3);
        local.env_versions.insert("dev".to_string(), 5);
        let remote = make_remote(&[("KEY", "old")]);
        // Remote at v4 — with env version 5 local is newer
        let result = reconcile(&local, &remote, 4, "dev");
        assert!(matches!(result.action, ReconcileAction::PushLocal));
    }

    #[test]
    fn multi_uses_env_version() {
        let mut local = make_payload(&[("A:dev", "old")], 10);
        local.env_versions.insert("dev".to_string(), 3);
        let remote = make_composite(&[("A:dev", "new")]);
        // Remote at v5, local env version is 3 — remote wins
        let result = reconcile_multi(&local, &[("op", &remote, 5)], Some("dev"));
        assert!(result.local_changed);
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "new");
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

    // --- reconcile_multi tests ---

    fn make_composite(secrets: &[(&str, &str)]) -> BTreeMap<String, String> {
        secrets
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn multi_local_highest_no_change() {
        let local = make_payload(&[("A:dev", "a")], 5);
        let remote = make_composite(&[("A:dev", "a")]);
        let result = reconcile_multi(&local, &[("op", &remote, 3)], None);
        assert!(!result.local_changed);
        assert_eq!(result.merged_payload.version, 5);
        assert!(result.sources_to_update.contains(&"op".to_string()));
    }

    #[test]
    fn multi_remote_highest_pulls() {
        let local = make_payload(&[("A:dev", "old")], 3);
        let remote = make_composite(&[("A:dev", "new")]);
        let result = reconcile_multi(&local, &[("op", &remote, 5)], None);
        assert!(result.local_changed);
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "new");
    }

    #[test]
    fn multi_two_remotes_highest_wins() {
        let local = make_payload(&[("A:dev", "local")], 1);
        let remote1 = make_composite(&[("A:dev", "r1")]);
        let remote2 = make_composite(&[("A:dev", "r2")]);
        let result = reconcile_multi(&local, &[("r1", &remote1, 3), ("r2", &remote2, 5)], None);
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "r2");
        assert!(result.sources_to_update.contains(&"r1".to_string()));
    }

    #[test]
    fn multi_merges_unique_secrets() {
        let local = make_payload(&[("A:dev", "a")], 3);
        let remote = make_composite(&[("B:dev", "b")]);
        let result = reconcile_multi(&local, &[("op", &remote, 5)], None);
        // Remote is higher, so B:dev pulled in. A:dev preserved from local.
        assert!(result.merged_payload.secrets.contains_key("A:dev"));
        assert!(result.merged_payload.secrets.contains_key("B:dev"));
        // Had merge so version is max+1
        assert_eq!(result.merged_payload.version, 6);
    }

    #[test]
    fn multi_all_same_version_no_change() {
        let local = make_payload(&[("A:dev", "a")], 5);
        let remote = make_composite(&[("A:dev", "a")]);
        let result = reconcile_multi(&local, &[("op", &remote, 5)], None);
        assert!(!result.local_changed);
        assert_eq!(result.merged_payload.version, 5);
        assert!(result.sources_to_update.is_empty());
    }

    #[test]
    fn multi_empty_remotes() {
        let local = make_payload(&[("A:dev", "a")], 5);
        let result = reconcile_multi(&local, &[], None);
        assert!(!result.local_changed);
        assert_eq!(result.merged_payload.version, 5);
    }

    #[test]
    fn multi_three_sources() {
        let local = make_payload(&[("L:dev", "l")], 1);
        let r1 = make_composite(&[("R1:dev", "r1")]);
        let r2 = make_composite(&[("R2:dev", "r2")]);
        let result = reconcile_multi(&local, &[("r1", &r1, 3), ("r2", &r2, 2)], None);
        // r1 at v3 is highest, so R1:dev is base. R2:dev merged as unique from lower version.
        // L:dev preserved from local.
        assert!(result.merged_payload.secrets.contains_key("L:dev"));
        assert!(result.merged_payload.secrets.contains_key("R1:dev"));
        assert!(result.merged_payload.secrets.contains_key("R2:dev"));
        // Had merges so version is max(3)+1 = 4
        assert_eq!(result.merged_payload.version, 4);
    }
}
