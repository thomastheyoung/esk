use anyhow::Result;
use std::collections::BTreeSet;

use crate::config::Config;
use crate::deploy_tracker::{DeployIndex, DeployStatus};
use crate::store::SecretStore;
use crate::sync_tracker::{SyncIndex, SyncStatus};

use super::types::*;

impl Dashboard {
    pub(crate) fn build(config: &Config, env: Option<&str>) -> Result<Self> {
        let store = SecretStore::open(&config.root)?;
        let payload = store.payload()?;
        let all_secrets = &payload.secrets;

        let index_path = config.root.join(".esk/deploy-index.json");
        let index = DeployIndex::load(&index_path);
        let resolved = config.resolve_secrets()?;
        let target_names: Vec<&str> = config.target_names();

        let filtered_env = env.map(String::from);

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
                        let current_hash = DeployIndex::hash_value(v);
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
                            validation_warnings.push(ValidationWarning {
                                key: secret.key.clone(),
                                env: env_name.to_string(),
                                message: e.message,
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
                let violations =
                    crate::validate::validate_cross_field(&cross_field_specs, all_secrets, env_name);
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
        let sync_index = SyncIndex::load(&sync_index_path);
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
                command: format!("esk set {} --env {}", v.key, v.env),
                description: v.message.clone(),
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
