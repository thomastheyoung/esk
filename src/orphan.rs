use std::collections::BTreeSet;

use crate::config::ResolvedSecret;
use crate::deploy_tracker::{DeployIndex, DeployStatus};

/// A secret that was previously deployed to a target but is no longer in the config.
#[derive(Debug, Clone)]
pub struct TargetOrphan {
    pub tracker_key: String,
    pub key: String,
    pub service: String,
    pub app: Option<String>,
    pub env: String,
    pub last_deployed_at: String,
}

impl TargetOrphan {
    /// Display the target as "service" or "service:app" (no env).
    pub fn target_display(&self) -> String {
        match &self.app {
            Some(a) => format!("{}:{}", self.service, a),
            None => self.service.clone(),
        }
    }
}

/// Detect orphaned deploy index records: records whose tracker key doesn't match
/// any (key, target, app, env) combination in the current resolved secrets.
///
/// Skips successful tombstones (already deleted). Includes failed tombstones
/// (deletion was attempted but failed).
pub fn detect(
    index: &DeployIndex,
    resolved: &[ResolvedSecret],
    env_filter: Option<&str>,
) -> Vec<TargetOrphan> {
    // Build expected tracker keys from resolved secrets
    let mut expected: BTreeSet<String> = BTreeSet::new();
    for secret in resolved {
        for target in &secret.targets {
            let tk = DeployIndex::tracker_key(
                &secret.key,
                &target.service,
                target.app.as_deref(),
                &target.environment,
            );
            expected.insert(tk);
        }
    }

    let mut orphans = Vec::new();
    for (tracker_key, record) in &index.records {
        if expected.contains(tracker_key) {
            continue;
        }

        // Skip successful tombstones (already cleaned up)
        if record.value_hash == DeployIndex::TOMBSTONE_HASH
            && record.last_deploy_status == DeployStatus::Success
        {
            continue;
        }

        // Parse tracker key: KEY:service:env or KEY:service:app:env
        let Some(parts) = DeployIndex::parse_tracker_key(tracker_key) else {
            continue;
        };

        if let Some(filter) = env_filter {
            if parts.env != filter {
                continue;
            }
        }

        orphans.push(TargetOrphan {
            tracker_key: tracker_key.clone(),
            key: parts.key,
            service: parts.service,
            app: parts.app,
            env: parts.env,
            last_deployed_at: record.last_deployed_at.clone(),
        });
    }

    orphans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ResolvedTarget;
    use crate::deploy_tracker::DeployIndex;
    use std::path::Path;

    fn make_resolved(key: &str, service: &str, app: Option<&str>, env: &str) -> ResolvedSecret {
        ResolvedSecret {
            key: key.to_string(),
            vendor: "Test".to_string(),
            description: None,
            targets: vec![ResolvedTarget {
                service: service.to_string(),
                app: app.map(|s| s.to_string()),
                environment: env.to_string(),
            }],
            validate: None,
            required: crate::config::Required::All,
            allow_empty: false,
        }
    }

    #[test]
    fn no_orphans_when_index_matches_config() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success(
            "API_KEY:fly:web:dev".to_string(),
            "fly:web:dev".to_string(),
            "hash1".to_string(),
        );
        let resolved = vec![make_resolved("API_KEY", "fly", Some("web"), "dev")];
        let orphans = detect(&index, &resolved, None);
        assert!(orphans.is_empty());
    }

    #[test]
    fn secret_removed_from_config_detected() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success(
            "OLD_KEY:fly:web:dev".to_string(),
            "fly:web:dev".to_string(),
            "hash1".to_string(),
        );
        let resolved = vec![make_resolved("API_KEY", "fly", Some("web"), "dev")];
        let orphans = detect(&index, &resolved, None);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].key, "OLD_KEY");
        assert_eq!(orphans[0].service, "fly");
        assert_eq!(orphans[0].app.as_deref(), Some("web"));
        assert_eq!(orphans[0].env, "dev");
    }

    #[test]
    fn target_removed_from_secret_detected() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        // Secret was deployed to both fly and cloudflare
        index.record_success(
            "API_KEY:fly:web:dev".to_string(),
            "fly:web:dev".to_string(),
            "hash1".to_string(),
        );
        index.record_success(
            "API_KEY:cloudflare:dev".to_string(),
            "cloudflare:dev".to_string(),
            "hash1".to_string(),
        );
        // Config now only has fly target
        let resolved = vec![make_resolved("API_KEY", "fly", Some("web"), "dev")];
        let orphans = detect(&index, &resolved, None);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].service, "cloudflare");
    }

    #[test]
    fn successful_tombstone_excluded() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success(
            "OLD_KEY:fly:web:dev".to_string(),
            "fly:web:dev".to_string(),
            DeployIndex::TOMBSTONE_HASH.to_string(),
        );
        let resolved: Vec<ResolvedSecret> = vec![];
        let orphans = detect(&index, &resolved, None);
        assert!(orphans.is_empty());
    }

    #[test]
    fn failed_tombstone_included() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_failure(
            "OLD_KEY:fly:web:dev".to_string(),
            "fly:web:dev".to_string(),
            DeployIndex::TOMBSTONE_HASH.to_string(),
            "timeout".to_string(),
        );
        let resolved: Vec<ResolvedSecret> = vec![];
        let orphans = detect(&index, &resolved, None);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].key, "OLD_KEY");
    }

    #[test]
    fn env_filter_applied() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success(
            "OLD_KEY:fly:web:dev".to_string(),
            "fly:web:dev".to_string(),
            "hash1".to_string(),
        );
        index.record_success(
            "OLD_KEY:fly:web:prod".to_string(),
            "fly:web:prod".to_string(),
            "hash1".to_string(),
        );
        let resolved: Vec<ResolvedSecret> = vec![];

        let orphans = detect(&index, &resolved, Some("prod"));
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].env, "prod");
    }

    #[test]
    fn empty_deploy_index_no_orphans() {
        let index = DeployIndex::new(Path::new("/tmp/test.json"));
        let resolved = vec![make_resolved("API_KEY", "fly", Some("web"), "dev")];
        let orphans = detect(&index, &resolved, None);
        assert!(orphans.is_empty());
    }

    #[test]
    fn empty_config_all_records_are_orphans() {
        let mut index = DeployIndex::new(Path::new("/tmp/test.json"));
        index.record_success(
            "KEY_A:fly:web:dev".to_string(),
            "fly:web:dev".to_string(),
            "hash1".to_string(),
        );
        index.record_success(
            "KEY_B:cloudflare:prod".to_string(),
            "cloudflare:prod".to_string(),
            "hash2".to_string(),
        );
        let resolved: Vec<ResolvedSecret> = vec![];
        let orphans = detect(&index, &resolved, None);
        assert_eq!(orphans.len(), 2);
    }
}
