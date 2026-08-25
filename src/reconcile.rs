use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use thiserror::Error;

use crate::store::StorePayload;

/// Controls which side wins when versions match but content has drifted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum ConflictPreference {
    /// Local store is source-of-truth at equal version (default)
    #[default]
    Local,
    /// Remote is source-of-truth at equal version
    Remote,
}

/// Maximum allowed version jump from local to remote. If a remote version
/// exceeds local by more than this, reconciliation fails to prevent a
/// compromised remote from overwriting all local secrets.
pub const MAX_VERSION_JUMP: u64 = 1000;

#[derive(Debug, Error)]
pub enum ReconcileError {
    #[error(
        "version jump too large: remote version {remote_version} exceeds local \
         {local_version} by {jump} (max allowed: {max_allowed_jump}). \
         This may indicate a compromised remote."
    )]
    VersionJump {
        local_version: u64,
        remote_version: u64,
        jump: u64,
        max_allowed_jump: u64,
    },
    #[error(
        "version jump too large: remote '{remote}' reports version {remote_version} \
         (local is {local_version}, jump of {jump}). Max allowed: {max_allowed_jump}. \
         This may indicate a compromised remote."
    )]
    RemoteVersionJump {
        remote: String,
        local_version: u64,
        remote_version: u64,
        jump: u64,
        max_allowed_jump: u64,
    },
}

pub fn is_version_jump_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ReconcileError>().is_some_and(|e| {
        matches!(
            e,
            ReconcileError::VersionJump { .. } | ReconcileError::RemoteVersionJump { .. }
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Remote is newer — pull remote secrets into local
    PullRemote,
    /// Local is newer — push local diffs to remote
    PushLocal,
    /// Versions are equal but content has drifted — push local to repair remote.
    /// Unlike `PushLocal`, this does NOT indicate a version advancement.
    RepairDrift,
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
) -> Result<ReconcileResult> {
    reconcile_with_jump_limit(local, remote_secrets, remote_version, env, true)
}

pub fn reconcile_with_jump_limit(
    local: &StorePayload,
    remote_secrets: &BTreeMap<String, String>,
    remote_version: u64,
    env: &str,
    enforce_jump_limit: bool,
) -> Result<ReconcileResult> {
    let local_env_secrets = extract_env_secrets(&local.secrets, env);

    let local_version = local.env_version(env);

    // Version jump protection
    if enforce_jump_limit && remote_version > local_version {
        let jump = remote_version - local_version;
        if jump > MAX_VERSION_JUMP {
            return Err(ReconcileError::VersionJump {
                local_version,
                remote_version,
                jump,
                max_allowed_jump: MAX_VERSION_JUMP,
            }
            .into());
        }
    }

    Ok(match local_version.cmp(&remote_version) {
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
                if local_val.map(std::string::String::as_str) != Some(value.as_str()) {
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
            let mut merged_env_last_changed_at = local.env_last_changed_at.clone();
            if merged_version != local_version {
                merged_env_last_changed_at.insert(env.to_string(), chrono::Utc::now().to_rfc3339());
            }

            let merged_payload = StorePayload {
                secrets: merged,
                version: merged_version.max(local.version),
                tombstones: merged_tombstones,
                env_versions: merged_env_versions,
                legacy_env_version_floor: local.inherited_legacy_env_version_floor(),
                env_last_changed_at: merged_env_last_changed_at,
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
            let pushed: Vec<String> = local_env_secrets
                .iter()
                .filter(|(key, value)| {
                    remote_secrets.get(*key).map(String::as_str) != Some(value.as_str())
                })
                .map(|(key, _)| key.clone())
                .collect();

            ReconcileResult {
                action: ReconcileAction::PushLocal,
                pulled: Vec::new(),
                pushed,
                merged_payload: None,
            }
        }
        std::cmp::Ordering::Equal => {
            // Equal version should still verify content to detect silent drift.
            // Prefer local as source-of-truth at equal version and push repair.
            let mut pushed = Vec::new();
            let mut drift = false;

            for (key, value) in &local_env_secrets {
                let remote_val = remote_secrets.get(key);
                if remote_val.map(std::string::String::as_str) != Some(value.as_str()) {
                    pushed.push(key.clone());
                    drift = true;
                }
            }
            if remote_secrets
                .keys()
                .any(|key| !local_env_secrets.contains_key(key))
            {
                drift = true;
            }

            if drift {
                ReconcileResult {
                    action: ReconcileAction::RepairDrift,
                    pulled: Vec::new(),
                    pushed,
                    merged_payload: None,
                }
            } else {
                ReconcileResult {
                    action: ReconcileAction::NoOp,
                    pulled: Vec::new(),
                    pushed: Vec::new(),
                    merged_payload: None,
                }
            }
        }
    })
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
    /// Names of sources that need updating.
    /// A source is marked stale if:
    /// - its version is behind the merged result, or
    /// - it reports the same version as merged but different content (drift).
    pub sources_to_update: Vec<String>,
    /// Whether anything changed compared to local.
    pub local_changed: bool,
    /// Whether any source had equal version but different content (drift repair).
    /// Unlike version-behind sources, drift sources don't require a local update.
    pub has_drift: bool,
}

#[derive(Clone, PartialEq, Eq)]
enum SnapshotCandidate {
    Value(String),
    Tombstone,
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
    env: &str,
    prefer: ConflictPreference,
) -> Result<MultiReconcileResult> {
    reconcile_multi_with_jump_limit(local, remotes, env, prefer, true)
}

pub fn reconcile_multi_with_jump_limit(
    local: &StorePayload,
    remotes: &[(&str, &BTreeMap<String, String>, u64)], // (name, composite_secrets, version)
    env: &str,
    prefer: ConflictPreference,
    enforce_jump_limit: bool,
) -> Result<MultiReconcileResult> {
    let local_version = local.env_version(env);

    // Find the max version across local and all remotes
    let mut max_version = local_version;
    for (name, _, version) in remotes {
        if *version > max_version {
            max_version = *version;
        }
        // Version jump protection for each remote
        if enforce_jump_limit && *version > local_version {
            let jump = *version - local_version;
            if jump > MAX_VERSION_JUMP {
                return Err(ReconcileError::RemoteVersionJump {
                    remote: (*name).to_string(),
                    local_version,
                    remote_version: *version,
                    jump,
                    max_allowed_jump: MAX_VERSION_JUMP,
                }
                .into());
            }
        }
    }

    // A version identifies one authoritative environment snapshot. If two
    // sources claim the same highest version but carry different snapshots,
    // there is no version-based winner. Refuse to let iteration order decide.
    if local_version < max_version {
        let highest: Vec<_> = remotes
            .iter()
            .filter(|(_, _, version)| *version == max_version)
            .collect();
        if let Some(first) = highest.first() {
            let first_scope = scoped_composite_secrets(first.1, env);
            let disagreeing: Vec<_> = highest
                .iter()
                .filter(|(_, secrets, _)| scoped_composite_secrets(secrets, env) != first_scope)
                .map(|(name, _, _)| *name)
                .collect();
            if !disagreeing.is_empty() {
                let mut names: Vec<_> = highest.iter().map(|(name, _, _)| *name).collect();
                names.sort_unstable();
                bail!(
                    "multiple remotes disagree at highest version v{max_version}: {}. \
                     Use --only <remote> to choose an authoritative source.",
                    names.join(", ")
                );
            }
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
                    if existing.map(std::string::String::as_str) != Some(value.as_str()) {
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

    // Determine which sources need updating (version-behind or equal-version drift).
    let mut has_drift = false;

    // When --prefer remote at equal version, pull remote content into merged.
    // Error if multiple remotes disagree at that version.
    if prefer == ConflictPreference::Remote && local_version == max_version {
        // Only applies when local == max_version (no version-based merge happened)
        let equal_version_remotes: Vec<_> = remotes
            .iter()
            .filter(|(_, _, v)| *v == final_version)
            .collect();

        if !equal_version_remotes.is_empty() {
            let merged_scope = scoped_composite_secrets(&merged, env);

            // Find remotes that actually drifted
            let drifted_remotes: Vec<_> = equal_version_remotes
                .iter()
                .filter(|(_, secrets, _)| scoped_composite_secrets(secrets, env) != merged_scope)
                .collect();

            if drifted_remotes.len() > 1 {
                // Check if the drifted remotes agree with each other
                let first_scope = scoped_composite_secrets(drifted_remotes[0].1, env);
                let all_agree = drifted_remotes[1..]
                    .iter()
                    .all(|(_, secrets, _)| scoped_composite_secrets(secrets, env) == first_scope);

                if !all_agree {
                    let names: Vec<_> = drifted_remotes.iter().map(|(n, _, _)| *n).collect();
                    bail!(
                        "multiple remotes disagree at equal version v{final_version}: {}. \
                         Use --only <remote> to choose which remote to prefer.",
                        names.join(", ")
                    );
                }
            }

            if !drifted_remotes.is_empty() {
                has_drift = true;
                // Pull the (agreed-upon) remote content into merged
                let (_, remote_secrets, _) = drifted_remotes[0];
                let suffix = format!(":{env}");
                for (key, value) in *remote_secrets {
                    if key.ends_with(&suffix) {
                        merged.insert(key.clone(), value.clone());
                        merged_tombstones.remove(key);
                    }
                }
                // Remove local-only keys in scope that remote doesn't have
                let local_scope_keys: Vec<_> = merged
                    .keys()
                    .filter(|k| k.ends_with(&suffix))
                    .cloned()
                    .collect();
                for key in local_scope_keys {
                    if !remote_secrets.contains_key(&key) {
                        merged.remove(&key);
                    }
                }
            }
        }
    }

    let merged_scope = scoped_composite_secrets(&merged, env);
    for (name, secrets, version) in remotes {
        let needs_update = match (*version).cmp(&final_version) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Equal => {
                let remote_scope = scoped_composite_secrets(secrets, env);
                let drifted = remote_scope != merged_scope;
                if drifted {
                    has_drift = true;
                }
                drifted
            }
            std::cmp::Ordering::Greater => false,
        };

        if needs_update {
            sources_to_update.push((*name).to_string());
        }
    }

    let local_changed = local_version != final_version || merged != local.secrets;

    let mut merged_env_versions = local.env_versions.clone();
    let mut merged_env_last_changed_at = local.env_last_changed_at.clone();
    merged_env_versions.insert(env.to_string(), final_version);
    if final_version != local_version {
        merged_env_last_changed_at.insert(env.to_string(), chrono::Utc::now().to_rfc3339());
    }

    Ok(MultiReconcileResult {
        merged_payload: StorePayload {
            secrets: merged,
            version: final_version.max(local.version),
            tombstones: merged_tombstones,
            env_versions: merged_env_versions,
            legacy_env_version_floor: local.inherited_legacy_env_version_floor(),
            env_last_changed_at: merged_env_last_changed_at,
        },
        sources_to_update,
        local_changed,
        has_drift,
    })
}

pub fn reconcile_multi_snapshots_with_jump_limit(
    local: &StorePayload,
    remotes: &[(&str, &crate::remotes::RemoteSnapshot)],
    env: &str,
    prefer: ConflictPreference,
    enforce_jump_limit: bool,
) -> Result<MultiReconcileResult> {
    for (_, snapshot) in remotes {
        snapshot.validate(env)?;
    }
    let local_version = local.env_version(env);
    let max_version = remotes
        .iter()
        .map(|(_, snapshot)| snapshot.version)
        .max()
        .unwrap_or(local_version)
        .max(local_version);
    for (name, snapshot) in remotes {
        if enforce_jump_limit && snapshot.version.saturating_sub(local_version) > MAX_VERSION_JUMP {
            return Err(ReconcileError::RemoteVersionJump {
                remote: (*name).to_string(),
                local_version,
                remote_version: snapshot.version,
                jump: snapshot.version - local_version,
                max_allowed_jump: MAX_VERSION_JUMP,
            }
            .into());
        }
    }

    let suffix = format!(":{env}");
    let local_scope = scoped_composite_secrets(&local.secrets, env);
    let local_tombstones: BTreeMap<String, u64> = local
        .tombstones
        .keys()
        .filter(|key| key.ends_with(&suffix))
        .map(|key| (key.clone(), local_version))
        .collect();
    let highest: Vec<_> = remotes
        .iter()
        .filter(|(_, snapshot)| snapshot.version == max_version)
        .collect();

    let snapshots_equal = |a: &crate::remotes::RemoteSnapshot,
                           b: &crate::remotes::RemoteSnapshot| {
        a.secrets == b.secrets && a.tombstones == b.tombstones
    };
    if local_version < max_version && highest.len() > 1 {
        let first = highest[0].1;
        if highest[1..]
            .iter()
            .any(|(_, snapshot)| !snapshots_equal(first, snapshot))
        {
            let mut names: Vec<_> = highest.iter().map(|(name, _)| *name).collect();
            names.sort_unstable();
            bail!(
                "multiple remotes disagree at highest version v{max_version}: {}. \
                 Use --only <remote> to choose an authoritative source.",
                names.join(", ")
            );
        }
    }

    let mut base_values = local_scope.clone();
    let mut base_tombstones = local_tombstones.clone();
    let mut base_is_remote = false;
    if local_version < max_version {
        if let Some((_, snapshot)) = highest.first() {
            base_values = snapshot.secrets.clone();
            base_tombstones = snapshot.tombstones.clone();
            base_is_remote = true;
        }
    } else if prefer == ConflictPreference::Remote {
        let drifted: Vec<_> = highest
            .iter()
            .filter(|(_, snapshot)| {
                snapshot.secrets != local_scope || snapshot.tombstones != local_tombstones
            })
            .collect();
        if let Some(first) = drifted.first() {
            if drifted[1..]
                .iter()
                .any(|(_, snapshot)| !snapshots_equal(first.1, snapshot))
            {
                let mut names: Vec<_> = drifted.iter().map(|(name, _)| *name).collect();
                names.sort_unstable();
                bail!(
                    "multiple remotes disagree at equal version v{max_version}: {}. \
                     Use --only <remote> to choose which remote to prefer.",
                    names.join(", ")
                );
            }
            // Explicit operator override: equal-version remote state, including
            // deletion or resurrection, replaces local scope.
            base_values = first.1.secrets.clone();
            base_tombstones = first.1.tombstones.clone();
            base_is_remote = true;
        }
    }

    let mut candidates: BTreeMap<String, (u64, SnapshotCandidate)> = BTreeMap::new();
    let mut add_candidate = |key: &str, version: u64, candidate: SnapshotCandidate| -> Result<()> {
        match candidates.get(key) {
            Some((current_version, _)) if *current_version > version => {}
            Some((current_version, current)) if *current_version == version => {
                match (current, &candidate) {
                    (SnapshotCandidate::Tombstone, _) => {}
                    (_, SnapshotCandidate::Tombstone) => {
                        candidates.insert(key.to_string(), (version, SnapshotCandidate::Tombstone));
                    }
                    (SnapshotCandidate::Value(left), SnapshotCandidate::Value(right))
                        if left != right =>
                    {
                        bail!("sources disagree for a key at equal version v{version}");
                    }
                    _ => {}
                }
            }
            _ => {
                candidates.insert(key.to_string(), (version, candidate));
            }
        }
        Ok(())
    };
    if base_is_remote && local_version < max_version {
        for (key, value) in &local_scope {
            add_candidate(key, local_version, SnapshotCandidate::Value(value.clone()))?;
        }
        for key in local_tombstones.keys() {
            add_candidate(key, local_version, SnapshotCandidate::Tombstone)?;
        }
    }
    for (_, snapshot) in remotes {
        if snapshot.version < max_version {
            for (key, value) in &snapshot.secrets {
                add_candidate(
                    key,
                    snapshot.version,
                    SnapshotCandidate::Value(value.clone()),
                )?;
            }
            for (key, version) in &snapshot.tombstones {
                add_candidate(key, *version, SnapshotCandidate::Tombstone)?;
            }
        }
    }

    let mut had_merge = false;
    for (key, (_, candidate)) in candidates {
        if base_values.contains_key(&key) || base_tombstones.contains_key(&key) {
            continue;
        }
        match candidate {
            SnapshotCandidate::Value(value) => {
                base_values.insert(key, value);
            }
            SnapshotCandidate::Tombstone => {
                base_tombstones.insert(key, max_version);
            }
        }
        had_merge = true;
    }
    let final_version = max_version
        .checked_add(u64::from(had_merge))
        .context("environment version overflow during reconciliation")?;
    if had_merge {
        for version in base_tombstones.values_mut() {
            *version = final_version;
        }
    }

    let mut merged = local.secrets.clone();
    merged.retain(|key, _| !key.ends_with(&suffix));
    merged.extend(base_values.clone());
    let mut merged_tombstones = local.tombstones.clone();
    merged_tombstones.retain(|key, _| !key.ends_with(&suffix));
    merged_tombstones.extend(base_tombstones.clone());

    let mut sources_to_update = Vec::new();
    let mut has_drift = false;
    for (name, snapshot) in remotes {
        if snapshot.version < final_version
            || snapshot.secrets != base_values
            || snapshot.tombstones != base_tombstones
        {
            sources_to_update.push((*name).to_string());
            if snapshot.version == final_version {
                has_drift = true;
            }
        }
    }
    let local_changed = local_version != final_version
        || local_scope != base_values
        || local_tombstones != base_tombstones;
    let mut env_versions = local.env_versions.clone();
    env_versions.insert(env.to_string(), final_version);
    let mut changed_at = local.env_last_changed_at.clone();
    if local_version != final_version {
        changed_at.insert(env.to_string(), chrono::Utc::now().to_rfc3339());
    }
    Ok(MultiReconcileResult {
        merged_payload: StorePayload {
            secrets: merged,
            version: local.version.max(final_version),
            tombstones: merged_tombstones,
            env_versions,
            legacy_env_version_floor: local.inherited_legacy_env_version_floor(),
            env_last_changed_at: changed_at,
        },
        sources_to_update,
        local_changed,
        has_drift,
    })
}

fn scoped_composite_secrets(
    secrets: &BTreeMap<String, String>,
    env: &str,
) -> BTreeMap<String, String> {
    let suffix = format!(":{env}");
    secrets
        .iter()
        .filter(|(k, _)| k.ends_with(&suffix))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn make_payload(secrets: &[(&str, &str)], version: u64) -> StorePayload {
        let mut map = BTreeMap::new();
        for (k, v) in secrets {
            map.insert((*k).to_string(), (*v).to_string());
        }
        StorePayload {
            secrets: map,
            version,
            ..Default::default()
        }
    }

    fn make_remote(secrets: &[(&str, &str)]) -> BTreeMap<String, String> {
        secrets
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn snapshot(
        secrets: &[(&str, &str)],
        tombstones: &[(&str, u64)],
        version: u64,
    ) -> crate::remotes::RemoteSnapshot {
        crate::remotes::RemoteSnapshot {
            secrets: secrets
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
            tombstones: tombstones
                .iter()
                .map(|(key, version)| ((*key).to_string(), *version))
                .collect(),
            version,
        }
    }

    #[test]
    fn typed_snapshot_deletion_converges_on_second_node() {
        let local = StorePayload {
            secrets: BTreeMap::from([("KEY:dev".to_string(), "old".to_string())]),
            version: 1,
            env_versions: BTreeMap::from([("dev".to_string(), 1)]),
            ..Default::default()
        };
        let deleted = snapshot(&[], &[("KEY:dev", 2)], 2);
        let result = reconcile_multi_snapshots_with_jump_limit(
            &local,
            &[("remote", &deleted)],
            "dev",
            ConflictPreference::Local,
            true,
        )
        .unwrap();
        assert!(!result.merged_payload.secrets.contains_key("KEY:dev"));
        assert_eq!(result.merged_payload.tombstones.get("KEY:dev"), Some(&2));
    }

    #[test]
    fn typed_snapshot_materialization_preserves_legacy_floor() {
        let local = make_payload(&[("PROD_KEY:prod", "local")], 10);
        let remote = snapshot(&[("DEV_KEY:dev", "remote")], &[], 11);

        let result = reconcile_multi_snapshots_with_jump_limit(
            &local,
            &[("remote", &remote)],
            "dev",
            ConflictPreference::Local,
            true,
        )
        .unwrap();

        assert_eq!(result.merged_payload.env_version("dev"), 11);
        assert_eq!(result.merged_payload.env_version("prod"), 10);
        assert_eq!(result.merged_payload.legacy_env_version_floor, Some(10));
    }

    #[test]
    fn typed_snapshot_resurrection_requires_strictly_newer_value() {
        let local = StorePayload {
            version: 2,
            tombstones: BTreeMap::from([("KEY:dev".to_string(), 2)]),
            env_versions: BTreeMap::from([("dev".to_string(), 2)]),
            ..Default::default()
        };
        let equal_value = snapshot(&[("KEY:dev", "value")], &[], 2);
        let kept = reconcile_multi_snapshots_with_jump_limit(
            &local,
            &[("remote", &equal_value)],
            "dev",
            ConflictPreference::Local,
            true,
        )
        .unwrap();
        assert!(!kept.merged_payload.secrets.contains_key("KEY:dev"));

        let newer_value = snapshot(&[("KEY:dev", "value")], &[], 3);
        let resurrected = reconcile_multi_snapshots_with_jump_limit(
            &local,
            &[("remote", &newer_value)],
            "dev",
            ConflictPreference::Local,
            true,
        )
        .unwrap();
        assert!(resurrected.merged_payload.secrets.contains_key("KEY:dev"));
        assert!(!resurrected
            .merged_payload
            .tombstones
            .contains_key("KEY:dev"));
    }

    #[test]
    fn lower_source_tombstone_uses_event_provenance_order_independently() {
        let local = StorePayload {
            secrets: BTreeMap::from([("KEY:dev".to_string(), "old".to_string())]),
            version: 3,
            env_versions: BTreeMap::from([("dev".to_string(), 3)]),
            ..Default::default()
        };
        let highest = snapshot(&[], &[], 5);
        let deleted = snapshot(&[], &[("KEY:dev", 4)], 4);
        for remotes in [
            vec![("high", &highest), ("deleted", &deleted)],
            vec![("deleted", &deleted), ("high", &highest)],
        ] {
            let result = reconcile_multi_snapshots_with_jump_limit(
                &local,
                &remotes,
                "dev",
                ConflictPreference::Local,
                true,
            )
            .unwrap();
            assert!(!result.merged_payload.secrets.contains_key("KEY:dev"));
            assert!(result.merged_payload.tombstones.contains_key("KEY:dev"));
        }
    }

    #[test]
    fn equal_highest_remotes_disagree_while_local_is_behind() {
        let local = make_payload(&[("KEY:dev", "old")], 1);
        let remote_a = make_remote(&[("KEY:dev", "a")]);
        let remote_b = make_remote(&[("KEY:dev", "b")]);

        let forward = reconcile_multi(
            &local,
            &[("a", &remote_a, 2), ("b", &remote_b, 2)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap_err()
        .to_string();
        let reverse = reconcile_multi(
            &local,
            &[("b", &remote_b, 2), ("a", &remote_a, 2)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap_err()
        .to_string();

        assert_eq!(forward, reverse);
        assert!(forward.contains("multiple remotes disagree at highest version v2: a, b"));
    }

    #[test]
    fn equal_highest_remotes_that_agree_are_order_independent() {
        let local = make_payload(&[("KEY:dev", "old")], 1);
        let remote_a = make_remote(&[("KEY:dev", "new")]);
        let remote_b = remote_a.clone();

        let forward = reconcile_multi(
            &local,
            &[("a", &remote_a, 2), ("b", &remote_b, 2)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        let reverse = reconcile_multi(
            &local,
            &[("b", &remote_b, 2), ("a", &remote_a, 2)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();

        assert_eq!(
            forward.merged_payload.secrets,
            reverse.merged_payload.secrets
        );
        assert_eq!(
            forward.merged_payload.env_versions,
            reverse.merged_payload.env_versions
        );
    }

    fn generated_composite_map(
        prefix: &'static str,
    ) -> impl Strategy<Value = BTreeMap<String, String>> {
        prop::collection::btree_map(any::<u8>(), any::<u32>(), 0..8).prop_map(move |entries| {
            entries
                .into_iter()
                .map(|(key, value)| (format!("{prefix}{key}:dev"), value.to_string()))
                .collect()
        })
    }

    proptest! {
        #[test]
        fn reconcile_multi_is_monotonic_and_order_invariant(
            local_entries in generated_composite_map("LOCAL"),
            remote_entries in generated_composite_map("REMOTE"),
            local_version in 0u64..10_000,
            remote_one_version in 0u64..10_000,
            remote_two_version in 0u64..10_000,
        ) {
            let local = StorePayload {
                secrets: local_entries,
                version: local_version,
                env_versions: BTreeMap::from([("dev".to_string(), local_version)]),
                ..Default::default()
            };
            let remote_one = remote_entries.clone();
            let remote_two = remote_entries;

            let forward = reconcile_multi_with_jump_limit(
                &local,
                &[
                    ("one", &remote_one, remote_one_version),
                    ("two", &remote_two, remote_two_version),
                ],
                "dev",
                ConflictPreference::Local,
                false,
            ).unwrap();
            let reverse = reconcile_multi_with_jump_limit(
                &local,
                &[
                    ("two", &remote_two, remote_two_version),
                    ("one", &remote_one, remote_one_version),
                ],
                "dev",
                ConflictPreference::Local,
                false,
            ).unwrap();

            prop_assert!(forward.merged_payload.version >= local.version);
            prop_assert!(forward.merged_payload.env_version("dev") >= local.env_version("dev"));
            prop_assert_eq!(forward.merged_payload.secrets, reverse.merged_payload.secrets);
            prop_assert_eq!(forward.merged_payload.tombstones, reverse.merged_payload.tombstones);
            prop_assert_eq!(forward.merged_payload.version, reverse.merged_payload.version);
            prop_assert_eq!(forward.merged_payload.env_versions, reverse.merged_payload.env_versions);
            prop_assert_eq!(forward.local_changed, reverse.local_changed);
            prop_assert_eq!(forward.has_drift, reverse.has_drift);

            let mut forward_updates = forward.sources_to_update;
            let mut reverse_updates = reverse.sources_to_update;
            forward_updates.sort();
            reverse_updates.sort();
            prop_assert_eq!(forward_updates, reverse_updates);
        }

        #[test]
        fn newer_value_beats_older_tombstone(tombstone_version in 0u64..10_000, version_delta in 1u64..1_000) {
            let remote_version = tombstone_version + version_delta;
            let local = StorePayload {
                version: tombstone_version,
                tombstones: BTreeMap::from([("KEY:dev".to_string(), tombstone_version)]),
                env_versions: BTreeMap::from([("dev".to_string(), tombstone_version)]),
                ..Default::default()
            };
            let remote = BTreeMap::from([("KEY:dev".to_string(), "remote".to_string())]);

            let result = reconcile_multi_with_jump_limit(
                &local,
                &[("remote", &remote, remote_version)],
                "dev",
                ConflictPreference::Local,
                false,
            ).unwrap();

            prop_assert_eq!(result.merged_payload.secrets.get("KEY:dev"), Some(&"remote".to_string()));
            prop_assert!(!result.merged_payload.tombstones.contains_key("KEY:dev"));
        }

        #[test]
        fn newer_tombstone_beats_older_value(tombstone_version in 1u64..10_000, version_delta in 0u64..1_000) {
            let remote_version = tombstone_version.saturating_sub(version_delta);
            let local = StorePayload {
                version: tombstone_version,
                tombstones: BTreeMap::from([("KEY:dev".to_string(), tombstone_version)]),
                env_versions: BTreeMap::from([("dev".to_string(), tombstone_version)]),
                ..Default::default()
            };
            let remote = BTreeMap::from([("KEY:dev".to_string(), "remote".to_string())]);

            let result = reconcile_multi_with_jump_limit(
                &local,
                &[("remote", &remote, remote_version)],
                "dev",
                ConflictPreference::Local,
                false,
            ).unwrap();

            prop_assert!(!result.merged_payload.secrets.contains_key("KEY:dev"));
            prop_assert_eq!(result.merged_payload.tombstones.get("KEY:dev"), Some(&tombstone_version));
        }
    }

    #[test]
    fn equal_versions_noop() {
        let local = make_payload(&[("KEY:dev", "val")], 5);
        let remote = make_remote(&[("KEY", "val")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::NoOp);
        assert!(result.pulled.is_empty());
        assert!(result.pushed.is_empty());
        assert!(result.merged_payload.is_none());
    }

    #[test]
    fn equal_versions_value_drift_returns_repair_drift() {
        let local = make_payload(&[("KEY:dev", "local")], 5);
        let remote = make_remote(&[("KEY", "remote")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::RepairDrift);
        assert_eq!(result.pushed, vec!["KEY"]);
    }

    #[test]
    fn equal_versions_remote_extra_key_detected_as_drift() {
        let local = make_payload(&[], 5);
        let remote = make_remote(&[("EXTRA", "x")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::RepairDrift);
    }

    #[test]
    fn local_newer_push_local() {
        let local = make_payload(&[("KEY:dev", "new_val")], 5);
        let remote = make_remote(&[("KEY", "old_val")]);
        let result = reconcile(&local, &remote, 3, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PushLocal);
        assert!(result.pulled.is_empty());
        assert_eq!(result.pushed, vec!["KEY"]);
    }

    #[test]
    fn remote_newer_pull_remote() {
        let local = make_payload(&[("KEY:dev", "old_val")], 3);
        let remote = make_remote(&[("KEY", "new_val")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
        assert_eq!(result.pulled, vec!["KEY"]);
        assert!(result.pushed.is_empty());
    }

    #[test]
    fn pull_remote_merges_new_secrets() {
        let local = make_payload(&[("A:dev", "a_val")], 3);
        let remote = make_remote(&[("A", "a_val"), ("B", "b_val")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
        assert_eq!(result.pulled, vec!["B"]);
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("B:dev").unwrap(), "b_val");
    }

    #[test]
    fn pull_remote_updates_existing() {
        let local = make_payload(&[("KEY:dev", "old")], 3);
        let remote = make_remote(&[("KEY", "new")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "new");
    }

    #[test]
    fn pull_remote_local_only_pushed() {
        let local = make_payload(&[("A:dev", "a"), ("LOCAL:dev", "local_val")], 3);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.pushed, vec!["LOCAL"]);
    }

    #[test]
    fn pull_remote_version_no_local_only() {
        let local = make_payload(&[("A:dev", "old")], 3);
        let remote = make_remote(&[("A", "new")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.version, 5);
    }

    #[test]
    fn pull_remote_version_with_local_only() {
        let local = make_payload(&[("A:dev", "a"), ("LOCAL:dev", "x")], 3);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.version, 6); // remote + 1
    }

    #[test]
    fn pull_remote_preserves_other_envs() {
        let local = make_payload(&[("KEY:dev", "dev_val"), ("KEY:prod", "prod_val")], 3);
        let remote = make_remote(&[("KEY", "new_dev")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "new_dev");
        assert_eq!(merged.secrets.get("KEY:prod").unwrap(), "prod_val");
    }

    #[test]
    fn push_local_computes_diff() {
        let local = make_payload(&[("A:dev", "same"), ("B:dev", "changed")], 5);
        let remote = make_remote(&[("A", "same"), ("B", "old")]);
        let result = reconcile(&local, &remote, 3, "dev").unwrap();
        assert_eq!(result.pushed, vec!["B"]);
    }

    #[test]
    fn push_local_same_values() {
        let local = make_payload(&[("A:dev", "val"), ("B:dev", "val2")], 5);
        let remote = make_remote(&[("A", "val"), ("B", "val2")]);
        let result = reconcile(&local, &remote, 3, "dev").unwrap();
        assert!(result.pushed.is_empty());
    }

    #[test]
    fn noop_empty_stores() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[]);
        let result = reconcile(&local, &remote, 0, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::NoOp);
    }

    #[test]
    fn pull_remote_empty_local() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[("A", "a"), ("B", "b")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
        assert_eq!(result.pulled.len(), 2);
    }

    #[test]
    fn push_local_empty_remote() {
        let local = make_payload(&[("A:dev", "a"), ("B:dev", "b")], 5);
        let remote = make_remote(&[]);
        let result = reconcile(&local, &remote, 3, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PushLocal);
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
            map.insert((*k).to_string(), (*v).to_string());
        }
        let mut tomb_map = BTreeMap::new();
        for (k, v) in tombstones {
            tomb_map.insert((*k).to_string(), *v);
        }
        StorePayload {
            secrets: map,
            version,
            tombstones: tomb_map,
            ..Default::default()
        }
    }

    #[test]
    fn tombstone_wins_when_newer_than_remote() {
        // Local deleted KEY at v4, remote has KEY at v3
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 4)], 4);
        let remote = make_remote(&[("KEY", "old_val")]);
        let result = reconcile(&local, &remote, 3, "dev").unwrap();
        // Local is newer: PushLocal, KEY stays deleted
        assert_eq!(result.action, ReconcileAction::PushLocal);
    }

    #[test]
    fn tombstone_loses_when_remote_newer() {
        // Local deleted KEY at v3, remote has KEY at v5
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 3)], 3);
        let remote = make_remote(&[("KEY", "new_val")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
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
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
        let merged = result.merged_payload.unwrap();
        assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "resurrected");
    }

    #[test]
    fn multi_tombstone_wins_over_lower_version_remote() {
        // Local deleted KEY:dev at v4
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 4)], 4);
        let remote = make_composite(&[("KEY:dev", "old")]);
        // Remote at v3 — tombstone at v4 should win
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 3)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert!(!result.merged_payload.secrets.contains_key("KEY:dev"));
        assert!(result.merged_payload.tombstones.contains_key("KEY:dev"));
    }

    #[test]
    fn multi_tombstone_loses_to_higher_version_remote() {
        // Local deleted KEY:dev at v3
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 3)], 3);
        let remote = make_composite(&[("KEY:dev", "new")]);
        // Remote at v5 — should resurrect
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert_eq!(result.merged_payload.secrets.get("KEY:dev").unwrap(), "new");
        assert!(!result.merged_payload.tombstones.contains_key("KEY:dev"));
    }

    #[test]
    fn reconcile_uses_env_version() {
        // Local global version is 5, but env-specific for dev is 3
        let mut local = make_payload(&[("KEY:dev", "old")], 5);
        local.env_versions.insert("dev".to_string(), 3);
        let remote = make_remote(&[("KEY", "new")]);
        // Remote at v4 — with env version 3 local is behind
        let result = reconcile(&local, &remote, 4, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
    }

    #[test]
    fn reconcile_env_version_local_newer() {
        let mut local = make_payload(&[("KEY:dev", "new")], 3);
        local.env_versions.insert("dev".to_string(), 5);
        let remote = make_remote(&[("KEY", "old")]);
        // Remote at v4 — with env version 5 local is newer
        let result = reconcile(&local, &remote, 4, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PushLocal);
    }

    #[test]
    fn multi_uses_env_version() {
        let mut local = make_payload(&[("A:dev", "old")], 10);
        local.env_versions.insert("dev".to_string(), 3);
        let remote = make_composite(&[("A:dev", "new")]);
        // Remote at v5, local env version is 3 — remote wins
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert!(result.local_changed);
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "new");
    }

    #[test]
    fn reconcile_v0_vs_v0() {
        let local = make_payload(&[("A:dev", "a")], 0);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 0, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::NoOp);
    }

    #[test]
    fn reconcile_v0_vs_v1() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, 1, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
        assert_eq!(result.pulled, vec!["A"]);
    }

    // --- reconcile_multi tests ---

    fn make_composite(secrets: &[(&str, &str)]) -> BTreeMap<String, String> {
        secrets
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn multi_local_highest_no_change() {
        let local = make_payload(&[("A:dev", "a")], 5);
        let remote = make_composite(&[("A:dev", "a")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 3)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert!(!result.local_changed);
        assert!(!result.has_drift);
        assert_eq!(result.merged_payload.version, 5);
        assert!(result.sources_to_update.contains(&"op".to_string()));
    }

    #[test]
    fn multi_remote_highest_pulls() {
        let local = make_payload(&[("A:dev", "old")], 3);
        let remote = make_composite(&[("A:dev", "new")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert!(result.local_changed);
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "new");
    }

    #[test]
    fn multi_two_remotes_highest_wins() {
        let local = make_payload(&[("A:dev", "local")], 1);
        let remote1 = make_composite(&[("A:dev", "r1")]);
        let remote2 = make_composite(&[("A:dev", "r2")]);
        let result = reconcile_multi(
            &local,
            &[("r1", &remote1, 3), ("r2", &remote2, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "r2");
        assert!(result.sources_to_update.contains(&"r1".to_string()));
    }

    #[test]
    fn multi_merges_unique_secrets() {
        let local = make_payload(&[("A:dev", "a")], 3);
        let remote = make_composite(&[("B:dev", "b")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
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
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert!(!result.local_changed);
        assert!(!result.has_drift);
        assert_eq!(result.merged_payload.version, 5);
        assert!(result.sources_to_update.is_empty());
    }

    #[test]
    fn multi_equal_version_content_drift_marks_source_stale() {
        let local = make_payload(&[("A:dev", "local")], 5);
        let remote = make_composite(&[("A:dev", "remote")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert!(!result.local_changed);
        assert!(result.has_drift);
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "local");
        assert_eq!(result.merged_payload.version, 5);
        assert_eq!(result.sources_to_update, vec!["op"]);
    }

    #[test]
    fn multi_equal_version_drift_outside_env_is_ignored() {
        let local = make_payload(&[("A:dev", "a"), ("B:prod", "local_prod")], 5);
        let remote = make_composite(&[("A:dev", "a"), ("B:prod", "remote_prod")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        assert!(!result.local_changed);
        assert!(!result.has_drift);
        assert!(result.sources_to_update.is_empty());
    }

    #[test]
    fn multi_empty_remotes() {
        let local = make_payload(&[("A:dev", "a")], 5);
        let result = reconcile_multi(&local, &[], "dev", ConflictPreference::Local).unwrap();
        assert!(!result.local_changed);
        assert_eq!(result.merged_payload.version, 5);
    }

    #[test]
    fn multi_three_sources() {
        let local = make_payload(&[("L:dev", "l")], 1);
        let r1 = make_composite(&[("R1:dev", "r1")]);
        let r2 = make_composite(&[("R2:dev", "r2")]);
        let result = reconcile_multi(
            &local,
            &[("r1", &r1, 3), ("r2", &r2, 2)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        // r1 at v3 is highest, so R1:dev is base. R2:dev merged as unique from lower version.
        // L:dev preserved from local.
        assert!(result.merged_payload.secrets.contains_key("L:dev"));
        assert!(result.merged_payload.secrets.contains_key("R1:dev"));
        assert!(result.merged_payload.secrets.contains_key("R2:dev"));
        // Had merges so version is max(3)+1 = 4
        assert_eq!(result.merged_payload.version, 4);
    }

    #[test]
    fn reconcile_version_never_regresses() {
        // Global version 10, env-specific dev at 3, remote at 5
        let mut local = make_payload(&[("KEY:dev", "old")], 10);
        local.env_versions.insert("dev".to_string(), 3);
        let remote = make_remote(&[("KEY", "new")]);
        let result = reconcile(&local, &remote, 5, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
        let merged = result.merged_payload.unwrap();
        // Global version must not drop below 10
        assert!(merged.version >= 10);
    }

    #[test]
    fn multi_reconcile_version_never_regresses() {
        // Global version 10, env-specific dev at 3, remote at 5
        let mut local = make_payload(&[("KEY:dev", "old")], 10);
        local.env_versions.insert("dev".to_string(), 3);
        let remote = make_composite(&[("KEY:dev", "new")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();
        // Global version must not drop below 10
        assert!(result.merged_payload.version >= 10);
    }

    // --- Phase 3b: version jump protection ---

    #[test]
    fn reconcile_rejects_large_version_jump() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[("A", "a")]);
        let err = reconcile(&local, &remote, MAX_VERSION_JUMP + 1, "dev").unwrap_err();
        assert!(err.to_string().contains("version jump too large"));
        assert!(is_version_jump_error(&err));
    }

    #[test]
    fn reconcile_accepts_version_jump_at_limit() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[("A", "a")]);
        let result = reconcile(&local, &remote, MAX_VERSION_JUMP, "dev").unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
    }

    #[test]
    fn reconcile_multi_rejects_large_version_jump() {
        let local = make_payload(&[], 0);
        let remote = make_composite(&[("A:dev", "a")]);
        let err = reconcile_multi(
            &local,
            &[("bad_remote", &remote, MAX_VERSION_JUMP + 1)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap_err();
        assert!(err.to_string().contains("version jump too large"));
        assert!(err.to_string().contains("bad_remote"));
        assert!(is_version_jump_error(&err));
    }

    #[test]
    fn reconcile_multi_rejects_jump_from_any_source() {
        let local = make_payload(&[], 5);
        let r1 = make_composite(&[("A:dev", "a")]);
        let r2 = make_composite(&[("B:dev", "b")]);
        let err = reconcile_multi(
            &local,
            &[("safe", &r1, 6), ("evil", &r2, MAX_VERSION_JUMP + 6)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap_err();
        assert!(err.to_string().contains("version jump too large"));
        assert!(err.to_string().contains("evil"));
        assert!(is_version_jump_error(&err));
    }

    #[test]
    fn reconcile_allows_large_version_jump_when_limit_disabled() {
        let local = make_payload(&[], 0);
        let remote = make_remote(&[("A", "a")]);
        let result =
            reconcile_with_jump_limit(&local, &remote, MAX_VERSION_JUMP + 1, "dev", false).unwrap();
        assert_eq!(result.action, ReconcileAction::PullRemote);
        assert_eq!(result.pulled, vec!["A"]);
    }

    #[test]
    fn reconcile_multi_allows_large_version_jump_when_limit_disabled() {
        let local = make_payload(&[], 0);
        let remote = make_composite(&[("A:dev", "a")]);
        let result = reconcile_multi_with_jump_limit(
            &local,
            &[("bad_remote", &remote, MAX_VERSION_JUMP + 1)],
            "dev",
            ConflictPreference::Local,
            false,
        )
        .unwrap();
        assert!(result.local_changed);
        assert_eq!(result.merged_payload.version, MAX_VERSION_JUMP + 2);
        assert!(result.merged_payload.secrets.contains_key("A:dev"));
    }

    #[test]
    fn reconcile_unknown_env_with_existing_env_versions_pulls_remote() {
        // Local has env_versions for dev/staging but not prod
        let mut local = make_payload(&[], 5);
        local.env_versions.insert("dev".to_string(), 5);
        local.env_versions.insert("staging".to_string(), 3);

        // Teammate pushed prod secrets at v2
        let remote = make_remote(&[("DB_URL", "postgres://prod")]);
        let result = reconcile(&local, &remote, 2, "prod").unwrap();

        // prod has no env version -> 0, so remote v2 wins
        assert_eq!(result.action, ReconcileAction::PullRemote);
        assert_eq!(result.pulled, vec!["DB_URL"]);
    }

    #[test]
    fn reconcile_materialization_preserves_legacy_floor_for_other_environments() {
        let local = make_payload(&[("PROD_KEY:prod", "local")], 10);
        let remote = make_remote(&[("DEV_KEY", "remote")]);

        let result = reconcile(&local, &remote, 11, "dev").unwrap();
        let merged = result.merged_payload.unwrap();

        assert_eq!(merged.env_version("dev"), 11);
        assert_eq!(merged.env_version("prod"), 10);
        assert_eq!(merged.legacy_env_version_floor, Some(10));
    }

    #[test]
    fn reconcile_multi_unknown_env_with_existing_env_versions_pulls_remote() {
        // Local has env_versions for dev but not prod
        let mut local = make_payload(&[], 5);
        local.env_versions.insert("dev".to_string(), 5);

        // Remote has prod secrets at v2
        let remote = make_composite(&[("DB_URL:prod", "postgres://prod")]);
        let result = reconcile_multi(
            &local,
            &[("remote1", &remote, 2)],
            "prod",
            ConflictPreference::Local,
        )
        .unwrap();

        // prod has no env version -> 0, so remote v2 is newer
        assert!(result.local_changed);
        assert!(result.merged_payload.secrets.contains_key("DB_URL:prod"));
    }

    #[test]
    fn reconcile_multi_materialization_preserves_legacy_floor() {
        let local = make_payload(&[("PROD_KEY:prod", "local")], 10);
        let remote = make_composite(&[("DEV_KEY:dev", "remote")]);

        let result = reconcile_multi(
            &local,
            &[("remote1", &remote, 11)],
            "dev",
            ConflictPreference::Local,
        )
        .unwrap();

        assert_eq!(result.merged_payload.env_version("dev"), 12);
        assert_eq!(result.merged_payload.env_version("prod"), 10);
        assert_eq!(result.merged_payload.legacy_env_version_floor, Some(10));
    }

    // --- --prefer remote tests ---

    #[test]
    fn multi_prefer_remote_pulls_remote_on_drift() {
        let local = make_payload(&[("A:dev", "local")], 5);
        let remote = make_composite(&[("A:dev", "remote")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(result.local_changed);
        assert!(result.has_drift);
        assert_eq!(
            result.merged_payload.secrets.get("A:dev").unwrap(),
            "remote"
        );
        // Remote is now in sync — should not need update
        assert!(result.sources_to_update.is_empty());
    }

    #[test]
    fn multi_prefer_remote_noop_when_no_drift() {
        let local = make_payload(&[("A:dev", "same")], 5);
        let remote = make_composite(&[("A:dev", "same")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(!result.local_changed);
        assert!(!result.has_drift);
        assert!(result.sources_to_update.is_empty());
    }

    #[test]
    fn multi_prefer_remote_removes_local_only_keys() {
        let local = make_payload(&[("A:dev", "a"), ("EXTRA:dev", "x")], 5);
        let remote = make_composite(&[("A:dev", "a")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(result.local_changed);
        assert!(!result.merged_payload.secrets.contains_key("EXTRA:dev"));
    }

    #[test]
    fn multi_prefer_remote_adds_remote_only_keys() {
        let local = make_payload(&[("A:dev", "a")], 5);
        let remote = make_composite(&[("A:dev", "a"), ("NEW:dev", "new")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(result.local_changed);
        assert_eq!(result.merged_payload.secrets.get("NEW:dev").unwrap(), "new");
    }

    #[test]
    fn multi_prefer_remote_preserves_other_env() {
        let local = make_payload(&[("A:dev", "local_dev"), ("A:prod", "local_prod")], 5);
        let remote = make_composite(&[("A:dev", "remote_dev")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert_eq!(
            result.merged_payload.secrets.get("A:dev").unwrap(),
            "remote_dev"
        );
        assert_eq!(
            result.merged_payload.secrets.get("A:prod").unwrap(),
            "local_prod"
        );
    }

    #[test]
    fn multi_prefer_remote_errors_when_remotes_disagree() {
        let local = make_payload(&[("A:dev", "local")], 5);
        let r1 = make_composite(&[("A:dev", "r1_val")]);
        let r2 = make_composite(&[("A:dev", "r2_val")]);
        let err = reconcile_multi(
            &local,
            &[("remote1", &r1, 5), ("remote2", &r2, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap_err();
        assert!(err.to_string().contains("multiple remotes disagree"));
        assert!(err.to_string().contains("--only"));
    }

    #[test]
    fn multi_prefer_remote_ok_when_remotes_agree() {
        let local = make_payload(&[("A:dev", "local")], 5);
        let r1 = make_composite(&[("A:dev", "agreed")]);
        let r2 = make_composite(&[("A:dev", "agreed")]);
        let result = reconcile_multi(
            &local,
            &[("remote1", &r1, 5), ("remote2", &r2, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(result.local_changed);
        assert_eq!(
            result.merged_payload.secrets.get("A:dev").unwrap(),
            "agreed"
        );
    }

    #[test]
    fn multi_prefer_remote_does_not_affect_version_based_merge() {
        // When remote is newer by version, --prefer remote shouldn't change behavior
        let local = make_payload(&[("A:dev", "old")], 3);
        let remote = make_composite(&[("A:dev", "new")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(result.local_changed);
        assert_eq!(result.merged_payload.secrets.get("A:dev").unwrap(), "new");
    }

    #[test]
    fn multi_prefer_remote_preserves_local_only_when_remote_version_higher() {
        // Remote is at a higher version but has a subset of local's keys.
        // --prefer remote should NOT delete local-only keys here — this is a
        // version-based merge, not equal-version drift.
        let local = make_payload(&[("A:dev", "a"), ("B:dev", "b")], 3);
        let remote = make_composite(&[("A:dev", "a")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(result.merged_payload.secrets.contains_key("B:dev"));
    }

    #[test]
    fn multi_prefer_remote_clears_tombstone_on_pull() {
        // Local deleted KEY at v5, remote re-added it at v5.
        // --prefer remote should clear the tombstone.
        let local = make_payload_with_tombstones(&[], &[("KEY:dev", 5)], 5);
        let remote = make_composite(&[("KEY:dev", "resurrected")]);
        let result = reconcile_multi(
            &local,
            &[("op", &remote, 5)],
            "dev",
            ConflictPreference::Remote,
        )
        .unwrap();
        assert!(result.local_changed);
        assert_eq!(
            result.merged_payload.secrets.get("KEY:dev").unwrap(),
            "resurrected"
        );
        assert!(!result.merged_payload.tombstones.contains_key("KEY:dev"));
    }
}
