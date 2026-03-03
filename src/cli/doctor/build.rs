use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::config::Config;
use crate::deploy_tracker::{DeployIndex, DeployStatus};
use crate::store::{KeyProvider, SecretStore, StorePayload};
use crate::sync_tracker::{SyncIndex, SyncStatus};

use super::types::{Check, Report, Section, Suggestion};

impl Report {
    /// Build a doctor report. Infallible — catches errors and records them as findings.
    pub(crate) fn build(root: &Path) -> Report {
        let esk_dir = root.join(".esk");
        let mut structure = Vec::new();
        let mut project_name: Option<String> = None;

        // 1. .esk/ directory exists
        let esk_dir_ok = esk_dir.is_dir();
        if esk_dir_ok {
            structure.push(Check::pass(".esk/ directory", "exists"));
        } else {
            structure.push(Check::fail(".esk/ directory", "missing — run `esk init`"));
        }

        // 2. esk.yaml exists and parses
        let config_path = root.join("esk.yaml");
        let config = if config_path.is_file() {
            structure.push(Check::pass("esk.yaml", "exists"));
            match Config::load(&config_path) {
                Ok(cfg) => {
                    structure.push(Check::pass("esk.yaml parses", "valid config"));
                    project_name = Some(cfg.project.clone());
                    Some(cfg)
                }
                Err(e) => {
                    structure.push(Check::fail("esk.yaml parses", format!("{e:#}")));
                    None
                }
            }
        } else {
            structure.push(Check::fail("esk.yaml", "missing — run `esk init`"));
            None
        };

        // 3. Key provider marker exists and is valid
        let provider = if esk_dir_ok {
            match KeyProvider::from_marker(&esk_dir) {
                Ok(p) => {
                    let desc = match &p {
                        KeyProvider::File { .. } => "file-based key",
                        KeyProvider::Keychain { .. } => "OS keychain",
                    };
                    structure.push(Check::pass("Key provider", desc));
                    Some(p)
                }
                Err(e) => {
                    structure.push(Check::fail("Key provider", format!("{e:#}")));
                    None
                }
            }
        } else {
            None
        };

        // 4. Encryption key loads
        let key_ok = if let Some(ref p) = provider {
            match p.load() {
                Ok(_) => {
                    structure.push(Check::pass("Encryption key", "loads successfully"));
                    true
                }
                Err(e) => {
                    structure.push(Check::fail("Encryption key", format!("{e:#}")));
                    false
                }
            }
        } else {
            false
        };

        // 5. Store decrypts
        let payload = if key_ok {
            match SecretStore::open(root) {
                Ok(store) => match store.payload() {
                    Ok(p) => {
                        let count = p.secrets.len();
                        structure.push(Check::pass(
                            "Store decrypts",
                            format!("{count} secret values"),
                        ));
                        Some(p)
                    }
                    Err(e) => {
                        structure.push(Check::fail("Store decrypts", format!("{e:#}")));
                        None
                    }
                },
                Err(e) => {
                    structure.push(Check::fail("Store opens", format!("{e:#}")));
                    None
                }
            }
        } else {
            None
        };

        // 6. .gitignore entries
        check_gitignore(root, &mut structure);

        // --- Config health ---
        let config_section = match &config {
            Some(cfg) => Section::Checked(build_config_health(cfg)),
            None => Section::Skipped("config did not load".into()),
        };

        // --- Store consistency ---
        let store_consistency = match (&config, &payload) {
            (Some(cfg), Some(p)) => Section::Checked(build_store_consistency(cfg, p)),
            _ => Section::Skipped("config or store unavailable".into()),
        };

        // --- Secrets health ---
        let secrets_health = match (&config, &payload) {
            (Some(cfg), Some(p)) => Section::Checked(build_secrets_health(cfg, p)),
            _ => Section::Skipped("config or store unavailable".into()),
        };

        // --- Suggestions ---
        let mut suggestions = Vec::new();
        build_suggestions(
            &structure,
            &config_section,
            &store_consistency,
            &secrets_health,
            &mut suggestions,
        );

        Report {
            project: project_name,
            root: root.to_path_buf(),
            structure,
            config: config_section,
            store_consistency,
            secrets_health,
            suggestions,
        }
    }
}

fn check_gitignore(root: &Path, structure: &mut Vec<Check>) {
    let gitignore_path = root.join(".esk/.gitignore");
    if !gitignore_path.is_file() {
        structure.push(Check::warn(
            ".esk/.gitignore",
            "missing — run `esk init` to create",
        ));
        return;
    }

    let contents = match std::fs::read_to_string(&gitignore_path) {
        Ok(c) => c,
        Err(e) => {
            structure.push(Check::warn(".esk/.gitignore", format!("unreadable: {e}")));
            return;
        }
    };

    let expected = crate::cli::init::ESK_GITIGNORE_ENTRIES;
    let missing: Vec<&str> = expected
        .iter()
        .filter(|entry| !contents.lines().any(|line| line.trim() == **entry))
        .copied()
        .collect();

    if missing.is_empty() {
        structure.push(Check::pass(
            ".esk/.gitignore",
            format!("all {} entries present", expected.len()),
        ));
    } else {
        structure.push(Check::warn(
            ".esk/.gitignore",
            format!(
                "missing {} of {} entries: {}",
                missing.len(),
                expected.len(),
                missing.join(", ")
            ),
        ));
    }
}

fn build_config_health(config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();

    let n_envs = config.environments.len();
    let n_secrets: usize = config.secrets.values().map(BTreeMap::len).sum();
    let n_apps = config.apps.len();
    let n_targets = config.typed_targets.len();
    let n_remotes = config.typed_remotes.len();

    checks.push(Check::pass(
        "Summary",
        format!(
            "{n_envs} environments, {n_secrets} secrets, {n_apps} apps, {n_targets} targets, {n_remotes} remotes"
        ),
    ));

    // Secrets with no targets
    for (group, secrets) in &config.secrets {
        for (key, def) in secrets {
            if def.targets.is_empty() {
                checks.push(Check::warn(
                    format!("{key} ({group})"),
                    "no targets configured",
                ));
            }
        }
    }

    checks
}

fn build_store_consistency(config: &Config, payload: &StorePayload) -> Vec<Check> {
    let mut checks = Vec::new();

    // Check composite key format
    let mut bad_keys = Vec::new();
    for key in payload.secrets.keys() {
        if key.rsplit_once(':').is_none() {
            bad_keys.push(key.clone());
        }
    }
    if bad_keys.is_empty() {
        checks.push(Check::pass(
            "Key format",
            "all keys have valid KEY:env format",
        ));
    } else {
        checks.push(Check::fail(
            "Key format",
            format!(
                "{} keys with invalid format: {}",
                bad_keys.len(),
                bad_keys.join(", ")
            ),
        ));
    }

    // Orphaned keys (in store but not in config)
    let config_keys: BTreeSet<&str> = config
        .secrets
        .values()
        .flat_map(|vs| vs.keys().map(String::as_str))
        .collect();

    let mut orphaned_keys = Vec::new();
    for composite in payload.secrets.keys() {
        if let Some((key, _)) = composite.rsplit_once(':') {
            if !config_keys.contains(key) {
                orphaned_keys.push(composite.clone());
            }
        }
    }

    if orphaned_keys.is_empty() {
        checks.push(Check::pass("Store keys", "all keys match config"));
    } else {
        checks.push(Check::warn(
            "Store orphans",
            format!("{} keys in store not in config", orphaned_keys.len()),
        ));
    }

    // Unknown environments in store
    let config_envs: BTreeSet<&str> = config.environments.iter().map(String::as_str).collect();
    let mut unknown_envs: BTreeSet<String> = BTreeSet::new();
    for composite in payload.secrets.keys() {
        if let Some((_, env)) = composite.rsplit_once(':') {
            if !config_envs.contains(env) {
                unknown_envs.insert(env.to_string());
            }
        }
    }

    if unknown_envs.is_empty() {
        checks.push(Check::pass(
            "Store environments",
            "all environments match config",
        ));
    } else {
        let envs: Vec<&str> = unknown_envs.iter().map(String::as_str).collect();
        checks.push(Check::warn(
            "Store environments",
            format!("unknown environments in store: {}", envs.join(", ")),
        ));
    }

    // Tombstone version sanity
    let bad_tombstones: Vec<&String> = payload
        .tombstones
        .iter()
        .filter(|(_, &v)| v > payload.version)
        .map(|(k, _)| k)
        .collect();

    if payload.tombstones.is_empty() {
        checks.push(Check::pass("Tombstones", "none"));
    } else if bad_tombstones.is_empty() {
        checks.push(Check::pass(
            "Tombstones",
            format!(
                "{} tombstones, all within version bounds",
                payload.tombstones.len()
            ),
        ));
    } else {
        checks.push(Check::fail(
            "Tombstones",
            format!(
                "{} tombstones exceed store version {}",
                bad_tombstones.len(),
                payload.version,
            ),
        ));
    }

    checks
}

fn build_secrets_health(config: &Config, payload: &StorePayload) -> Vec<Check> {
    let mut checks = Vec::new();
    let index_path = config.root.join(".esk/deploy-index.json");
    let index = DeployIndex::load(&index_path);

    let resolved = match config.resolve_secrets() {
        Ok(r) => r,
        Err(e) => {
            checks.push(Check::fail("Resolve secrets", format!("{e:#}")));
            return checks;
        }
    };

    let target_names: Vec<&str> = config.target_names();

    // 1. Failed deployments
    let mut failed_count = 0usize;
    for secret in &resolved {
        for target in &secret.targets {
            if !target_names.contains(&target.service.as_str()) {
                continue;
            }
            let tracker_key = DeployIndex::tracker_key(
                &secret.key,
                &target.service,
                target.app.as_deref(),
                &target.environment,
            );
            if let Some(record) = index.records.get(&tracker_key) {
                if record.last_deploy_status == DeployStatus::Failed {
                    failed_count += 1;
                }
            }
        }
    }

    if failed_count > 0 {
        checks.push(Check::fail(
            "Failed deploys",
            format!("{failed_count} deployment(s) in failed state"),
        ));
    } else {
        checks.push(Check::pass("Failed deploys", "none"));
    }

    // 2. Missing required secrets
    let missing_required =
        config.check_requirements(&resolved, &payload.secrets, None, Some(&target_names));

    if missing_required.is_empty() {
        checks.push(Check::pass("Required secrets", "all present"));
    } else {
        checks.push(Check::warn(
            "Required secrets",
            format!("{} required secret(s) missing", missing_required.len()),
        ));
    }

    // 3. Validation violations
    let envs: Vec<&str> = config.environments.iter().map(String::as_str).collect();
    let mut validation_count = 0usize;
    for secret in &resolved {
        if let Some(ref spec) = secret.validate {
            for &env_name in &envs {
                let composite = format!("{}:{}", secret.key, env_name);
                if let Some(value) = payload.secrets.get(&composite) {
                    if crate::validate::validate_value(&secret.key, value, spec).is_err() {
                        validation_count += 1;
                    }
                }
            }
        }
    }

    if validation_count > 0 {
        checks.push(Check::warn(
            "Validation",
            format!("{validation_count} value(s) fail validation"),
        ));
    } else {
        checks.push(Check::pass("Validation", "all values valid"));
    }

    // 4. Cross-field violations
    let mut cross_field_specs: BTreeMap<&str, &crate::validate::Validation> = BTreeMap::new();
    for secret in &resolved {
        if let Some(ref spec) = secret.validate {
            if spec.has_cross_field_rules() {
                cross_field_specs.insert(secret.key.as_str(), spec);
            }
        }
    }
    let mut cross_field_count = 0usize;
    if !cross_field_specs.is_empty() {
        for &env_name in &envs {
            let violations = crate::validate::validate_cross_field(
                &cross_field_specs,
                &payload.secrets,
                env_name,
            );
            cross_field_count += violations.len();
        }
    }

    if cross_field_count > 0 {
        checks.push(Check::warn(
            "Cross-field rules",
            format!("{cross_field_count} violation(s)"),
        ));
    }

    // 5. Stale remote sync
    let sync_index_path = config.root.join(".esk/sync-index.json");
    let sync_index = SyncIndex::load(&sync_index_path);
    let remote_names: Vec<&String> = config.remotes.keys().collect();
    let mut stale_count = 0usize;
    let mut failed_sync_count = 0usize;

    for remote_name in &remote_names {
        for &env_name in &envs {
            let local_version = payload.env_version(env_name);
            let key = SyncIndex::tracker_key(remote_name, env_name);
            if let Some(record) = sync_index.records.get(&key) {
                if record.last_push_status == SyncStatus::Failed {
                    failed_sync_count += 1;
                } else if record.pushed_version < local_version {
                    stale_count += 1;
                }
            }
        }
    }

    if failed_sync_count > 0 {
        checks.push(Check::fail(
            "Remote sync",
            format!("{failed_sync_count} remote(s) in failed state"),
        ));
    }
    if stale_count > 0 {
        checks.push(Check::warn(
            "Remote sync",
            format!("{stale_count} remote(s) behind local"),
        ));
    }
    if failed_sync_count == 0 && stale_count == 0 && !remote_names.is_empty() {
        checks.push(Check::pass("Remote sync", "all remotes up to date"));
    }

    // 6. Target orphans
    let target_orphans = crate::orphan::detect(&index, &resolved, None);
    if target_orphans.is_empty() {
        checks.push(Check::pass("Target orphans", "none"));
    } else {
        checks.push(Check::warn(
            "Target orphans",
            format!(
                "{} deployed secret(s) no longer in config",
                target_orphans.len()
            ),
        ));
    }

    checks
}

fn build_suggestions(
    structure: &[Check],
    config_section: &Section,
    _store_consistency: &Section,
    secrets_health: &Section,
    suggestions: &mut Vec<Suggestion>,
) {
    use super::types::CheckStatus;

    // Structure failures
    for check in structure {
        if check.status == CheckStatus::Fail {
            if check.label.contains("esk.yaml") || check.label.contains(".esk/") {
                suggestions.push(Suggestion {
                    command: "esk init".into(),
                    reason: format!("{}: {}", check.label, check.detail),
                });
            }
            if check.label.contains("Encryption key") {
                suggestions.push(Suggestion {
                    command: "esk init".into(),
                    reason: "encryption key failed to load".into(),
                });
            }
        }
        if check.status == CheckStatus::Warn && check.label == ".esk/.gitignore" {
            suggestions.push(Suggestion {
                command: "esk init".into(),
                reason: "create .esk/.gitignore".into(),
            });
        }
    }

    // Config warnings (no targets)
    if let Section::Checked(checks) = config_section {
        for check in checks {
            if check.status == CheckStatus::Warn && check.detail == "no targets configured" {
                suggestions.push(Suggestion {
                    command: "esk set <KEY> --env <ENV>".into(),
                    reason: format!("configure targets for {}", check.label),
                });
            }
        }
    }

    // Secrets health
    if let Section::Checked(checks) = secrets_health {
        for check in checks {
            if check.status == CheckStatus::Fail && check.label == "Failed deploys" {
                suggestions.push(Suggestion {
                    command: "esk deploy".into(),
                    reason: "retry failed deployments".into(),
                });
            }
            if check.status == CheckStatus::Warn && check.label == "Required secrets" {
                suggestions.push(Suggestion {
                    command: "esk set <KEY> --env <ENV>".into(),
                    reason: "set missing required secrets".into(),
                });
            }
            if (check.status == CheckStatus::Warn || check.status == CheckStatus::Fail)
                && check.label == "Remote sync"
            {
                suggestions.push(Suggestion {
                    command: "esk sync".into(),
                    reason: check.detail.clone(),
                });
            }
            if check.status == CheckStatus::Warn && check.label == "Target orphans" {
                suggestions.push(Suggestion {
                    command: "esk deploy --prune".into(),
                    reason: "clean up orphaned deployments".into(),
                });
            }
        }
    }

    // Deduplicate by command
    let mut seen = BTreeSet::new();
    suggestions.retain(|s| seen.insert(s.command.clone()));
}
