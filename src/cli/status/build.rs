use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::deploy_tracker::{DeployIndex, DeployStatus};
use crate::store::SecretStore;
use crate::sync_tracker::{SyncIndex, SyncStatus};
use crate::targets::SecretValue;

use super::types::{
    CoverageGap, Dashboard, DeployEntry, EmptyValueWarning, NextStep, Orphan, RemoteState,
    RemoteStatus, ValidationWarning,
};

/// Whether the file a filesystem target wrote still matches the store.
///
/// `status` must not call a secret deployed when the artifact holding it was
/// deleted or edited, but it also has to stay fast and offline. Only targets
/// whose state is a local file are checked; everything else answers `None`,
/// because esk genuinely cannot tell without a network round-trip.
///
/// The comparison is the same one `deploy` performs, so the two commands
/// cannot disagree about whether an artifact is current. Results are cached per
/// target group: one file backs every secret in the group, so re-reading it per
/// secret would be wasted work.
fn dotenv_artifact_matches(
    config: &Config,
    service: &str,
    app: Option<&str>,
    env: &str,
    resolved: &[crate::config::ResolvedSecret],
    all_secrets: &BTreeMap<String, String>,
    cache: &mut BTreeMap<(String, String, String), Option<bool>>,
) -> Option<bool> {
    if service != ".env" {
        return None;
    }
    let app = app?;
    // Keyed by service as well as app and env: only `.env` reaches this point
    // today, but a second filesystem target must not silently share an entry.
    let key = (service.to_string(), app.to_string(), env.to_string());
    if let Some(cached) = cache.get(&key) {
        return *cached;
    }

    // The artifact holds every secret configured for this group, so the check
    // has to compare against the whole set rather than one key at a time.
    let mut secrets: Vec<SecretValue> = Vec::new();
    for secret in resolved {
        for target in &secret.targets {
            if target.service != service
                || target.app.as_deref() != Some(app)
                || target.environment != env
            {
                continue;
            }
            if let Some(value) = all_secrets.get(&format!("{}:{}", secret.key, env)) {
                secrets.push(SecretValue {
                    key: secret.key.clone(),
                    value: zeroize::Zeroizing::new(value.clone()),
                    group: secret.group.clone(),
                });
            }
        }
    }

    let result = if secrets.is_empty() {
        // Nothing stored for this group, so no artifact was ever written.
        None
    } else {
        let target = crate::config::ResolvedTarget {
            service: service.to_string(),
            app: Some(app.to_string()),
            environment: env.to_string(),
        };
        crate::targets::dotenv::DotenvTarget { config }.artifact_matches_readonly(&secrets, &target)
    };
    cache.insert(key, result);
    result
}

impl Dashboard {
    pub(crate) fn build(config: &Config, env: Option<&str>) -> Result<Self> {
        let store = SecretStore::open(&config.root)?;
        let payload = store.payload()?;
        let all_secrets = &payload.secrets;

        let index_path = config.root.join(".esk/deploy-index.json");
        let (index, warning) = DeployIndex::load(&index_path);
        if let Some(msg) = warning {
            let _ = cliclack::log::warning(&msg);
        }
        let resolved = config.resolve_secrets()?;
        let target_names: Vec<&str> = config.target_names();

        let filtered_env = env.map(String::from);

        // Artifact state per (app, env) for filesystem targets, resolved once
        // per group rather than per secret. `None` means esk cannot tell.
        let mut artifact_state: BTreeMap<(String, String, String), Option<bool>> = BTreeMap::new();

        let envs: Vec<&str> = match env {
            Some(e) => vec![e],
            None => config
                .environments
                .iter()
                .map(std::string::String::as_str)
                .collect(),
        };

        // Deploy entries
        let mut failed = Vec::new();
        let mut pending = Vec::new();
        let mut deployed = Vec::new();
        let mut unset = Vec::new();

        for secret in &resolved {
            for target in &secret.targets {
                if !envs.contains(&target.environment.as_str()) {
                    continue;
                }
                if !target_names.contains(&target.service.as_str()) {
                    continue;
                }

                let composite = format!("{}:{}", secret.key, target.environment);
                let value = all_secrets.get(&composite);
                let tracker_key = DeployIndex::tracker_key(
                    &secret.key,
                    &target.service,
                    target.app.as_deref(),
                    &target.environment,
                );

                let record = index.records.get(&tracker_key);

                let entry = DeployEntry {
                    key: secret.key.clone(),
                    env: target.environment.clone(),
                    target: target.target_display(),
                    error: record.and_then(|r| r.last_error.clone()),
                    last_deployed_at: record.map(|r| r.last_deployed_at.clone()),
                };

                match (value, record) {
                    (None, _) => unset.push(entry),
                    (Some(_), None) => pending.push(entry),
                    (Some(v), Some(rec)) => {
                        let current_hash = DeployIndex::hash_value(v, store.master_key());
                        if rec.last_deploy_status == DeployStatus::Failed {
                            failed.push(DeployEntry {
                                error: Some(
                                    rec.last_error
                                        .as_deref()
                                        .unwrap_or("unknown error")
                                        .to_string(),
                                ),
                                ..entry
                            });
                        } else if current_hash != rec.value_hash {
                            pending.push(DeployEntry {
                                last_deployed_at: Some(rec.last_deployed_at.clone()),
                                ..entry
                            });
                        } else if dotenv_artifact_matches(
                            config,
                            &target.service,
                            target.app.as_deref(),
                            &target.environment,
                            &resolved,
                            all_secrets,
                            &mut artifact_state,
                        ) == Some(false)
                        {
                            // The store is unchanged, but the file esk wrote is
                            // gone or altered, so this is not deployed state.
                            pending.push(DeployEntry {
                                last_deployed_at: Some(rec.last_deployed_at.clone()),
                                ..entry
                            });
                        } else {
                            deployed.push(entry);
                        }
                    }
                }
            }
        }

        // 3. Validation warnings
        let mut validation_warnings = Vec::new();
        for secret in &resolved {
            if let Some(ref spec) = secret.validate {
                for &env_name in &envs {
                    let composite = format!("{}:{}", secret.key, env_name);
                    if let Some(value) = all_secrets.get(&composite) {
                        if let Err(e) = crate::validate::validate_value(&secret.key, value, spec) {
                            let message = e.message();
                            validation_warnings.push(ValidationWarning {
                                key: secret.key.clone(),
                                env: env_name.to_string(),
                                message,
                                violations: e.into_violations(),
                            });
                        }
                    }
                }
            }
        }

        // 3b. Cross-field violations
        let mut cross_field_violations = Vec::new();
        let mut cross_field_specs: std::collections::BTreeMap<&str, &crate::validate::Validation> =
            std::collections::BTreeMap::new();
        for secret in &resolved {
            if let Some(ref spec) = secret.validate {
                if spec.has_cross_field_rules() {
                    cross_field_specs.insert(secret.key.as_str(), spec);
                }
            }
        }
        if !cross_field_specs.is_empty() {
            for &env_name in &envs {
                let violations = crate::validate::validate_cross_field(
                    &cross_field_specs,
                    all_secrets,
                    env_name,
                );
                cross_field_violations.extend(violations);
            }
        }

        // 4. Empty value warnings
        let mut empty_values = Vec::new();
        for secret in &resolved {
            if secret.allow_empty {
                continue;
            }
            for &env_name in &envs {
                let composite = format!("{}:{}", secret.key, env_name);
                if let Some(value) = all_secrets.get(&composite) {
                    if crate::validate::is_effectively_empty(value) {
                        empty_values.push(EmptyValueWarning {
                            key: secret.key.clone(),
                            env: env_name.to_string(),
                            kind: if value.is_empty() {
                                "empty"
                            } else {
                                "whitespace-only"
                            },
                        });
                    }
                }
            }
        }

        // 5. Required secret checks
        let missing_required =
            config.check_requirements(&resolved, all_secrets, env, Some(&target_names));

        // 6. Coverage gaps: secrets declared in config but missing values in some envs
        let mut coverage_gaps = Vec::new();
        for secret in &resolved {
            let secret_envs: BTreeSet<&str> = secret
                .targets
                .iter()
                .map(|t| t.environment.as_str())
                .collect();

            let mut missing_envs = Vec::new();
            let mut present_envs = Vec::new();

            for &e in &secret_envs {
                if !envs.contains(&e) {
                    continue;
                }
                let composite = format!("{}:{}", secret.key, e);
                if all_secrets.contains_key(&composite) {
                    present_envs.push(e.to_string());
                } else {
                    missing_envs.push(e.to_string());
                }
            }

            if !missing_envs.is_empty() && !present_envs.is_empty() {
                coverage_gaps.push(CoverageGap {
                    key: secret.key.clone(),
                    missing_envs,
                    present_envs,
                });
            }
        }

        // 7. Orphans: secrets in store but not in config
        let config_keys: BTreeSet<&str> = config
            .secrets
            .values()
            .flat_map(|vs| vs.keys().map(std::string::String::as_str))
            .collect();

        let mut orphans = Vec::new();
        for composite_key in all_secrets.keys() {
            if let Some((key, e)) = composite_key.rsplit_once(':') {
                if !envs.contains(&e) {
                    continue;
                }
                if !config_keys.contains(key) {
                    orphans.push(Orphan {
                        key: key.to_string(),
                        env: e.to_string(),
                    });
                }
            }
        }

        // 7b. Target orphans: deployed but no longer in config
        let target_orphans = crate::orphan::detect(&index, &resolved, env);

        // 8. Remote states
        let sync_index_path = config.root.join(".esk/sync-index.json");
        let (sync_index, warning) = SyncIndex::load(&sync_index_path);
        if let Some(msg) = warning {
            let _ = cliclack::log::warning(&msg);
        }
        let remote_names: Vec<&String> = config.remotes.keys().collect();

        let mut remote_states = Vec::new();
        for remote_name in &remote_names {
            for &env_name in &envs {
                let local_version = payload.env_version(env_name);
                let key = SyncIndex::tracker_key(remote_name, env_name);
                let status = match sync_index.records.get(&key) {
                    Some(record) if record.last_push_status == SyncStatus::Failed => {
                        RemoteStatus::Failed {
                            version: record.pushed_version,
                            error: record
                                .last_error
                                .as_deref()
                                .unwrap_or("unknown error")
                                .to_string(),
                        }
                    }
                    Some(record) if record.pushed_version >= local_version => {
                        RemoteStatus::Current {
                            version: local_version,
                        }
                    }
                    Some(record) => RemoteStatus::Stale {
                        pushed: record.pushed_version,
                        local: local_version,
                    },
                    None => RemoteStatus::NeverSynced,
                };
                remote_states.push(RemoteState {
                    name: (*remote_name).clone(),
                    env: env_name.to_string(),
                    status,
                });
            }
        }

        // 9. Next steps
        let mut next_steps = Vec::new();

        // Failed deploys
        for entry in &failed {
            next_steps.push(NextStep {
                command: format!("esk deploy --env {}", entry.env),
                description: format!("retry failed deploy for {}:{}", entry.key, entry.env),
            });
        }

        // Validation warnings
        for w in &validation_warnings {
            next_steps.push(NextStep {
                command: format!("esk set {} --env {}", w.key, w.env),
                description: format!("fix: {}", w.message),
            });
        }

        // Cross-field violations
        for v in &cross_field_violations {
            next_steps.push(NextStep {
                command: format!("esk set {} --env {}", v.key(), v.env()),
                description: v.message().to_string(),
            });
        }

        // Empty values
        for w in &empty_values {
            next_steps.push(NextStep {
                command: format!("esk set {} --env {}", w.key, w.env),
                description: format!("{} value (may break defaults)", w.kind),
            });
        }

        // Missing required secrets
        for m in &missing_required {
            next_steps.push(NextStep {
                command: format!("esk set {} --env {}", m.key, m.env),
                description: "required secret missing".to_string(),
            });
        }

        // Pending deploys (dedupe by env)
        let mut pending_envs: BTreeSet<&str> = BTreeSet::new();
        for entry in &pending {
            pending_envs.insert(&entry.env);
        }
        for env_name in &pending_envs {
            let count = pending.iter().filter(|e| e.env == **env_name).count();
            next_steps.push(NextStep {
                command: format!("esk deploy --env {env_name}"),
                description: format!(
                    "deploy {count} pending change{}",
                    if count == 1 { "" } else { "s" }
                ),
            });
        }

        // Coverage gaps
        for gap in &coverage_gaps {
            for missing_env in &gap.missing_envs {
                next_steps.push(NextStep {
                    command: format!("esk set {} --env {}", gap.key, missing_env),
                    description: "fill coverage gap".to_string(),
                });
            }
        }

        // Stale remotes
        for ps in &remote_states {
            if let RemoteStatus::Stale { pushed, local } = &ps.status {
                next_steps.push(NextStep {
                    command: format!("esk sync --env {}", ps.env),
                    description: format!(
                        "remote is {} version{} behind",
                        local - pushed,
                        if local - pushed == 1 { "" } else { "s" }
                    ),
                });
            }
            if let RemoteStatus::NeverSynced = &ps.status {
                next_steps.push(NextStep {
                    command: format!("esk sync --env {}", ps.env),
                    description: "remote never synced".to_string(),
                });
            }
        }

        // Store orphans
        for orphan in &orphans {
            next_steps.push(NextStep {
                command: format!("esk delete {} --env {}", orphan.key, orphan.env),
                description: "remove orphaned secret from store".to_string(),
            });
        }

        // Target orphans (dedupe by env)
        {
            let mut prune_envs: BTreeSet<&str> = BTreeSet::new();
            for o in &target_orphans {
                prune_envs.insert(&o.env);
            }
            for env_name in prune_envs {
                let count = target_orphans.iter().filter(|o| o.env == env_name).count();
                next_steps.push(NextStep {
                    command: format!("esk deploy --prune --env {env_name}"),
                    description: format!(
                        "prune {count} orphaned deploy{}",
                        if count == 1 { "" } else { "s" }
                    ),
                });
            }
        }

        // Deduplicate next steps by command
        let mut seen = BTreeSet::new();
        next_steps.retain(|s| seen.insert(s.command.clone()));

        let env_versions: Vec<(String, u64)> = envs
            .iter()
            .map(|e| ((*e).to_string(), payload.env_version(e)))
            .collect();

        Ok(Dashboard {
            project: config.project.clone(),
            version: payload.version,
            filtered_env,
            env_versions,
            failed,
            pending,
            deployed,
            unset,
            validation_warnings,
            cross_field_violations,
            empty_values,
            missing_required,
            coverage_gaps,
            orphans,
            target_orphans,
            remote_states,
            next_steps,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::targets::{DeployTarget, SecretValue};

    const DOTENV_YAML: &str = r"
project: testapp
environments: [dev]
apps:
  web:
    path: apps/web
targets:
  .env:
    pattern: '{app_path}/.env{env_suffix}.local'
    env_suffix:
      dev: ''
secrets:
  General:
    MY_SECRET:
      description: test
      targets:
        .env: [web:dev]
";

    /// A deleted artifact must move its secret out of `deployed` and into
    /// `pending`, rather than leaving a green check the store cannot support.
    #[test]
    fn missing_artifact_reclassifies_deployed_as_pending() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("esk.yaml"), DOTENV_YAML).unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("MY_SECRET", "dev", "val1").unwrap();
        let config = Config::load(&dir.path().join("esk.yaml")).unwrap();

        // Write the artifact and record the deploy exactly as a real run would.
        let target = crate::config::ResolvedTarget {
            service: ".env".to_string(),
            app: Some("web".to_string()),
            environment: "dev".to_string(),
        };
        let dotenv = crate::targets::dotenv::DotenvTarget { config: &config };
        dotenv
            .deploy_batch(
                &[SecretValue {
                    key: "MY_SECRET".to_string(),
                    value: zeroize::Zeroizing::new("val1".to_string()),
                    group: "General".to_string(),
                }],
                &target,
            )
            .unwrap();
        let index_path = dir.path().join(".esk/deploy-index.json");
        let (mut index, _) = DeployIndex::load(&index_path);
        index.record_success(
            DeployIndex::tracker_key("MY_SECRET", ".env", Some("web"), "dev"),
            target.to_string(),
            DeployIndex::hash_value("val1", store.master_key()),
        );
        index.save().unwrap();

        let dashboard = Dashboard::build(&config, Some("dev")).unwrap();
        assert_eq!(dashboard.deployed.len(), 1, "baseline: reported as sent");
        assert!(dashboard.pending.is_empty());

        std::fs::remove_file(dir.path().join("apps/web/.env.local")).unwrap();

        let dashboard = Dashboard::build(&config, Some("dev")).unwrap();
        assert!(
            dashboard.deployed.is_empty(),
            "a secret whose artifact was deleted is not deployed"
        );
        assert_eq!(dashboard.pending.len(), 1, "it needs redeploying");
    }

    /// A corrupted artifact behind a symlinked parent must still be caught.
    ///
    /// esk never *writes* through a symlink, but reading through one to see
    /// what is there cannot damage it. Declining to look would report an
    /// artifact esk never inspected as sent — and monorepos symlink app
    /// directories routinely, so the blind spot would be common.
    #[cfg(unix)]
    #[test]
    fn corrupted_artifact_behind_symlinked_parent_is_pending() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("esk.yaml"), DOTENV_YAML).unwrap();
        std::fs::create_dir_all(dir.path().join("packages_web")).unwrap();
        std::fs::create_dir_all(dir.path().join("apps")).unwrap();
        std::os::unix::fs::symlink("../packages_web", dir.path().join("apps/web")).unwrap();

        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("MY_SECRET", "dev", "val1").unwrap();
        let config = Config::load(&dir.path().join("esk.yaml")).unwrap();

        // Write the artifact into the real directory and record the deploy.
        let artifact = dir.path().join("packages_web/.env.local");
        let content = crate::targets::dotenv::render_dotenv_content(&[SecretValue {
            key: "MY_SECRET".to_string(),
            value: zeroize::Zeroizing::new("val1".to_string()),
            group: "General".to_string(),
        }])
        .unwrap();
        std::fs::write(&artifact, &content).unwrap();
        let index_path = dir.path().join(".esk/deploy-index.json");
        let (mut index, _) = DeployIndex::load(&index_path);
        index.record_success(
            DeployIndex::tracker_key("MY_SECRET", ".env", Some("web"), "dev"),
            ".env:web:dev".to_string(),
            DeployIndex::hash_value("val1", store.master_key()),
        );
        index.save().unwrap();

        let dashboard = Dashboard::build(&config, Some("dev")).unwrap();
        assert_eq!(dashboard.deployed.len(), 1, "baseline: reported as sent");

        std::fs::write(&artifact, "MY_SECRET=ATTACKER\n").unwrap();

        let dashboard = Dashboard::build(&config, Some("dev")).unwrap();
        assert!(
            dashboard.deployed.is_empty(),
            "a corrupted artifact behind a symlink is not sent"
        );
        assert_eq!(dashboard.pending.len(), 1);
    }

    /// The artifact is read once per group, not once per secret.
    ///
    /// A group's secrets all live in one file, so an uncached check would
    /// re-read and re-render it for every key in the group.
    #[test]
    fn artifact_check_is_cached_per_group() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("esk.yaml"), DOTENV_YAML).unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("MY_SECRET", "dev", "val1").unwrap();
        let config = Config::load(&dir.path().join("esk.yaml")).unwrap();
        let resolved = config.resolve_secrets().unwrap();
        let payload = store.payload().unwrap();

        let mut cache = BTreeMap::new();
        let first = dotenv_artifact_matches(
            &config,
            ".env",
            Some("web"),
            "dev",
            &resolved,
            &payload.secrets,
            &mut cache,
        );
        assert_eq!(cache.len(), 1, "the first call populates the cache");

        // Delete the file: a second call must return the cached answer rather
        // than re-reading and reporting something different.
        std::fs::remove_file(dir.path().join("apps/web/.env.local")).ok();
        let second = dotenv_artifact_matches(
            &config,
            ".env",
            Some("web"),
            "dev",
            &resolved,
            &payload.secrets,
            &mut cache,
        );
        assert_eq!(first, second, "the cached answer must be reused");
        assert_eq!(cache.len(), 1, "and no second entry created");
    }

    /// An edited artifact must be caught too, not just a deleted one.
    ///
    /// Checking only for the file's presence would report a file full of wrong
    /// values as sent — the same overclaiming this check exists to remove.
    #[test]
    fn edited_artifact_reclassifies_deployed_as_pending() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("esk.yaml"), DOTENV_YAML).unwrap();
        std::fs::create_dir_all(dir.path().join("apps/web")).unwrap();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("MY_SECRET", "dev", "val1").unwrap();
        let config = Config::load(&dir.path().join("esk.yaml")).unwrap();

        let target = crate::config::ResolvedTarget {
            service: ".env".to_string(),
            app: Some("web".to_string()),
            environment: "dev".to_string(),
        };
        let dotenv = crate::targets::dotenv::DotenvTarget { config: &config };
        dotenv
            .deploy_batch(
                &[SecretValue {
                    key: "MY_SECRET".to_string(),
                    value: zeroize::Zeroizing::new("val1".to_string()),
                    group: "General".to_string(),
                }],
                &target,
            )
            .unwrap();
        let index_path = dir.path().join(".esk/deploy-index.json");
        let (mut index, _) = DeployIndex::load(&index_path);
        index.record_success(
            DeployIndex::tracker_key("MY_SECRET", ".env", Some("web"), "dev"),
            target.to_string(),
            DeployIndex::hash_value("val1", store.master_key()),
        );
        index.save().unwrap();

        let env_path = dir.path().join("apps/web/.env.local");
        let mut perms = std::fs::metadata(&env_path).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            perms.set_mode(0o600);
        }
        #[cfg(not(unix))]
        perms.set_readonly(false);
        std::fs::set_permissions(&env_path, perms).unwrap();
        std::fs::write(&env_path, "MY_SECRET=TOTALLY_WRONG\n").unwrap();

        let dashboard = Dashboard::build(&config, Some("dev")).unwrap();
        assert!(
            dashboard.deployed.is_empty(),
            "a secret whose artifact holds a different value is not deployed"
        );
        assert_eq!(dashboard.pending.len(), 1, "it needs redeploying");
    }
}
