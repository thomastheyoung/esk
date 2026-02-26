use anyhow::Result;
use console::style;
use std::collections::BTreeSet;

use crate::config::Config;
use crate::deploy_tracker::{DeployIndex, DeployStatus};
use crate::remotes::{check_remote_health, RemoteHealth};
use crate::store::SecretStore;
use crate::sync_tracker::{SyncIndex, SyncStatus};
use crate::targets::{check_target_health, CommandRunner, RealCommandRunner, TargetHealth};
use crate::ui;
use crate::validate;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn run(config: &Config, env: Option<&str>, all: bool) -> Result<()> {
    run_with_runner(config, env, all, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    env: Option<&str>,
    all: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    let dashboard = Dashboard::build(config, env, runner)?;
    dashboard.render(all)
}

// ---------------------------------------------------------------------------
// Dashboard data model
// ---------------------------------------------------------------------------

struct DeployEntry {
    key: String,
    env: String,
    target: String,
    error: Option<String>,
    last_deployed_at: Option<String>,
}

struct CoverageGap {
    key: String,
    missing_envs: Vec<String>,
    present_envs: Vec<String>,
}

struct Orphan {
    key: String,
    env: String,
}

#[derive(Clone)]
enum RemoteStatus {
    Current { version: u64 },
    Stale { pushed: u64, local: u64 },
    Failed { version: u64, error: String },
    NeverSynced,
}

struct RemoteState {
    name: String,
    env: String,
    status: RemoteStatus,
}

struct ValidationWarning {
    key: String,
    env: String,
    message: String,
}

struct EmptyValueWarning {
    key: String,
    env: String,
    kind: &'static str,
}

struct NextStep {
    command: String,
    description: String,
}

struct Dashboard {
    project: String,
    version: u64,
    filtered_env: Option<String>,
    env_versions: Vec<(String, u64)>,
    target_health: Vec<TargetHealth>,
    #[allow(dead_code)]
    remote_health: Vec<RemoteHealth>,
    failed: Vec<DeployEntry>,
    pending: Vec<DeployEntry>,
    deployed: Vec<DeployEntry>,
    unset: Vec<DeployEntry>,
    validation_warnings: Vec<ValidationWarning>,
    cross_field_violations: Vec<validate::CrossFieldViolation>,
    empty_values: Vec<EmptyValueWarning>,
    missing_required: Vec<crate::config::MissingRequirement>,
    coverage_gaps: Vec<CoverageGap>,
    orphans: Vec<Orphan>,
    target_orphans: Vec<crate::orphan::TargetOrphan>,
    remote_states: Vec<RemoteState>,
    next_steps: Vec<NextStep>,
}

impl Dashboard {
    fn build(config: &Config, env: Option<&str>, runner: &dyn CommandRunner) -> Result<Self> {
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

        // 1. Health checks
        let target_health = check_target_health(config, runner);
        let remote_health = check_remote_health(config, runner);

        // 2. Deploy entries
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
        let mut cross_field_specs: std::collections::BTreeMap<&str, &validate::Validation> =
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
                    validate::validate_cross_field(&cross_field_specs, all_secrets, env_name);
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
                    name: (*remote_name).to_string(),
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
            .map(|e| (e.to_string(), payload.env_version(e)))
            .collect();

        Ok(Dashboard {
            project: config.project.clone(),
            version: payload.version,
            filtered_env,
            env_versions,
            target_health,
            remote_health,
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

    fn render(&self, all: bool) -> Result<()> {
        // When filtering a single env, show that env's version; otherwise global
        let display_version = match &self.filtered_env {
            Some(env) => self
                .env_versions
                .iter()
                .find(|(e, _)| e == env)
                .map_or(self.version, |(_, v)| *v),
            None => self.version,
        };

        // Summary line
        let total = self.failed.len() + self.pending.len() + self.deployed.len() + self.unset.len();
        let summary = if total == 0 {
            format!(
                "{} · {}",
                style(&self.project).bold(),
                style(format!("v{}", display_version)).dim(),
            )
        } else {
            let parts = ui::format_count_summary(&[
                ("deployed", self.deployed.len()),
                ("pending", self.pending.len()),
                ("failed", self.failed.len()),
                ("unset", self.unset.len()),
                ("invalid", self.validation_warnings.len()),
                ("cross-field", self.cross_field_violations.len()),
                ("empty", self.empty_values.len()),
                ("required missing", self.missing_required.len()),
                ("target orphans", self.target_orphans.len()),
            ]);

            let all_deployed = self.failed.is_empty()
                && self.pending.is_empty()
                && self.unset.is_empty()
                && self.validation_warnings.is_empty()
                && self.cross_field_violations.is_empty()
                && self.empty_values.is_empty()
                && self.missing_required.is_empty()
                && self.target_orphans.is_empty();

            if all_deployed {
                format!(
                    "{} · {} · {}, all deployed",
                    style(&self.project).bold(),
                    style(format!("v{}", display_version)).dim(),
                    style(format!(
                        "{total} target{}",
                        if total == 1 { "" } else { "s" }
                    )),
                )
            } else {
                format!(
                    "{} · {} · {} target{} ({})",
                    style(&self.project).bold(),
                    style(format!("v{}", display_version)).dim(),
                    total,
                    if total == 1 { "" } else { "s" },
                    parts,
                )
            }
        };

        cliclack::intro(style(summary).to_string())?;

        // Targets section
        if !self.target_health.is_empty() {
            let lines: Vec<String> = self
                .target_health
                .iter()
                .map(|h| {
                    let icon = if h.ok {
                        ui::icon_success()
                    } else {
                        ui::icon_failure()
                    };
                    format!("  {} {:<14} {}", icon, h.name, style(&h.message).dim())
                })
                .collect();
            cliclack::log::step(format!("Targets\n{}", lines.join("\n")))?;
        }

        // Deploy section
        let has_problems =
            !self.failed.is_empty() || !self.pending.is_empty() || !self.unset.is_empty();

        if has_problems || (all && !self.deployed.is_empty()) {
            let mut deploy_lines = Vec::new();

            if !self.failed.is_empty() {
                deploy_lines.push(ui::section_header(
                    ui::icon_failure(),
                    &format!("{} failed", self.failed.len()),
                    ui::SectionColor::Red,
                ));
                for entry in &self.failed {
                    let freshness = entry
                        .last_deployed_at
                        .as_deref()
                        .map(ui::format_relative_time)
                        .unwrap_or_default();
                    let err_text = entry
                        .error
                        .as_deref()
                        .map(|e| format!(" {}", style(format!("({e})")).dim()))
                        .unwrap_or_default();
                    deploy_lines.push(ui::section_entry(
                        &format!("{}:{}", entry.key, entry.env),
                        &format!("→ {}  {}{}", entry.target, style(freshness).dim(), err_text,),
                    ));
                }
            }

            if !self.pending.is_empty() {
                deploy_lines.push(ui::section_header(
                    ui::icon_pending(),
                    &format!("{} pending", self.pending.len()),
                    ui::SectionColor::Yellow,
                ));
                for entry in &self.pending {
                    let freshness = match &entry.last_deployed_at {
                        Some(t) => {
                            let ago = ui::format_relative_time(t);
                            if ago.is_empty() {
                                "never deployed".to_string()
                            } else {
                                format!("last deployed {ago}")
                            }
                        }
                        None => "never deployed".to_string(),
                    };
                    deploy_lines.push(ui::section_entry(
                        &format!("{}:{}", entry.key, entry.env),
                        &format!("→ {}  {}", entry.target, style(freshness).dim()),
                    ));
                }
            }

            if !self.unset.is_empty() {
                deploy_lines.push(ui::section_header(
                    ui::icon_unset(),
                    &format!("{} unset", self.unset.len()),
                    ui::SectionColor::Dim,
                ));
                for entry in &self.unset {
                    deploy_lines.push(ui::section_entry(
                        &format!("{}:{}", entry.key, entry.env),
                        &format!("→ {}", style(&entry.target).dim()),
                    ));
                }
            }

            if !self.deployed.is_empty() {
                if all {
                    deploy_lines.push(ui::section_header(
                        ui::icon_success(),
                        &format!("{} deployed", self.deployed.len()),
                        ui::SectionColor::Green,
                    ));
                    for entry in &self.deployed {
                        let freshness = entry
                            .last_deployed_at
                            .as_deref()
                            .map(ui::format_relative_time)
                            .unwrap_or_default();
                        deploy_lines.push(ui::section_entry(
                            &format!("{}:{}", entry.key, entry.env),
                            &format!("→ {}  {}", entry.target, style(freshness).dim()),
                        ));
                    }
                } else {
                    deploy_lines.push(format!(
                        "  {} {}  {}",
                        ui::icon_success(),
                        style(format!("{} deployed", self.deployed.len())).green(),
                        style("(--all to show)").dim()
                    ));
                }
            }

            if !deploy_lines.is_empty() {
                cliclack::log::step(format!("Deploy (targets)\n{}", deploy_lines.join("\n")))?;
            }
        }

        // Validation section
        let has_validation =
            !self.validation_warnings.is_empty() || !self.cross_field_violations.is_empty();
        if has_validation {
            let mut val_lines = Vec::new();
            if !self.validation_warnings.is_empty() {
                val_lines.push(ui::section_header(
                    ui::icon_alert_yellow(),
                    &format!("{} invalid", self.validation_warnings.len()),
                    ui::SectionColor::Yellow,
                ));
                for w in &self.validation_warnings {
                    val_lines.push(ui::section_entry(
                        &format!("{}:{}", w.key, w.env),
                        &style(&w.message).dim().to_string(),
                    ));
                }
            }
            if !self.cross_field_violations.is_empty() {
                val_lines.push(ui::section_header(
                    ui::icon_alert_yellow(),
                    &format!("{} cross-field", self.cross_field_violations.len()),
                    ui::SectionColor::Yellow,
                ));
                for v in &self.cross_field_violations {
                    val_lines.push(ui::section_entry(
                        &format!("{}:{}", v.key, v.env),
                        &style(&v.message).dim().to_string(),
                    ));
                }
            }
            cliclack::log::step(format!("Validation\n{}", val_lines.join("\n")))?;
        }

        // Empty values section
        if !self.empty_values.is_empty() {
            let mut empty_lines = Vec::new();
            empty_lines.push(ui::section_header(
                ui::icon_alert_yellow(),
                &format!("{} empty", self.empty_values.len()),
                ui::SectionColor::Yellow,
            ));
            for w in &self.empty_values {
                empty_lines.push(ui::section_entry(
                    &format!("{}:{}", w.key, w.env),
                    &style(w.kind).dim().to_string(),
                ));
            }
            cliclack::log::step(format!("Empty values\n{}", empty_lines.join("\n")))?;
        }

        // Required section
        if !self.missing_required.is_empty() {
            let mut req_lines = Vec::new();
            req_lines.push(ui::section_header(
                ui::icon_alert_red(),
                &format!("{} required missing", self.missing_required.len()),
                ui::SectionColor::Red,
            ));
            for m in &self.missing_required {
                let target_info = if m.targets.is_empty() {
                    String::new()
                } else {
                    format!("  {}", style(format!("({})", m.targets.join(", "))).dim())
                };
                req_lines.push(ui::section_entry(
                    &format!("{}:{}", m.key, m.env),
                    &target_info,
                ));
            }
            cliclack::log::step(format!("Requirements\n{}", req_lines.join("\n")))?;
        }

        // Coverage section
        let has_coverage = !self.coverage_gaps.is_empty()
            || !self.orphans.is_empty()
            || !self.target_orphans.is_empty();
        if has_coverage {
            let mut cov_lines = Vec::new();

            if !self.coverage_gaps.is_empty() {
                cov_lines.push(ui::section_header(
                    ui::icon_unset(),
                    &format!("{} declared but never set", self.coverage_gaps.len()),
                    ui::SectionColor::Dim,
                ));
                for gap in &self.coverage_gaps {
                    let present = gap.present_envs.join(", ");
                    for missing_env in &gap.missing_envs {
                        cov_lines.push(ui::section_entry(
                            &gap.key,
                            &format!(
                                "missing in {} {}",
                                style(missing_env).yellow(),
                                style(format!("(set in {present})")).dim(),
                            ),
                        ));
                    }
                }
            }

            if !self.orphans.is_empty() {
                cov_lines.push(ui::section_header(
                    ui::icon_warning(),
                    &format!("{} in store, not in config", self.orphans.len()),
                    ui::SectionColor::Yellow,
                ));
                for orphan in &self.orphans {
                    cov_lines.push(ui::section_entry(
                        &format!("{}:{}", orphan.key, orphan.env),
                        "",
                    ));
                }
            }

            if !self.target_orphans.is_empty() {
                cov_lines.push(ui::section_header(
                    ui::icon_warning(),
                    &format!(
                        "{} deployed but no longer in config",
                        self.target_orphans.len()
                    ),
                    ui::SectionColor::Yellow,
                ));
                for orphan in &self.target_orphans {
                    let target_display = orphan.target_display();
                    let freshness = ui::format_relative_time(&orphan.last_deployed_at);
                    cov_lines.push(ui::section_entry(
                        &orphan.key,
                        &format!(
                            "{} {}  {}",
                            style("→").dim(),
                            style(format!("{} ({})", target_display, orphan.env)),
                            style(freshness).dim(),
                        ),
                    ));
                }
            }

            cliclack::log::step(format!("Coverage\n{}", cov_lines.join("\n")))?;
        }

        // Sync section (remotes)
        if !self.remote_states.is_empty() {
            let lines: Vec<String> = self
                .remote_states
                .iter()
                .map(|ps| match &ps.status {
                    RemoteStatus::Current { version } => format!(
                        "  {} {}  {}",
                        ui::icon_success(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style(format!("v{version}")).dim()
                    ),
                    RemoteStatus::Stale { pushed, local } => format!(
                        "  {} {}  {}",
                        ui::icon_pending(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style(format!("v{pushed} → local v{local}")).dim()
                    ),
                    RemoteStatus::Failed { version, error } => format!(
                        "  {} {}  {} {}",
                        ui::icon_failure(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style(format!("v{version}")).dim(),
                        style(format!("({error})")).dim()
                    ),
                    RemoteStatus::NeverSynced => format!(
                        "  {} {}  {}",
                        ui::icon_unset(),
                        style(format!("{}:{}", ps.name, ps.env)),
                        style("never synced").dim()
                    ),
                })
                .collect();
            cliclack::log::step(format!("Sync (remotes)\n{}", lines.join("\n")))?;
        }

        // Next steps section
        if !self.next_steps.is_empty() {
            let cmd_width = self
                .next_steps
                .iter()
                .map(|s| s.command.len())
                .max()
                .unwrap_or(0);

            let lines: Vec<String> = self
                .next_steps
                .iter()
                .map(|s| {
                    format!(
                        "  {}  {}",
                        style(format!("{:<width$}", s.command, width = cmd_width)).cyan(),
                        style(&s.description).dim()
                    )
                })
                .collect();
            cliclack::log::step(format!("Next steps\n{}", lines.join("\n")))?;
        }

        let outro_text = match &self.filtered_env {
            Some(env) => format!("Store version: {} ({})", display_version, env),
            None if self.env_versions.is_empty() => {
                format!("Store version: {}", display_version)
            }
            None => {
                let parts: Vec<String> = self
                    .env_versions
                    .iter()
                    .map(|(e, v)| format!("{}: v{}", e, v))
                    .collect();
                format!("Store version: {} ({})", display_version, parts.join(", "))
            }
        };
        cliclack::outro(style(outro_text).dim().to_string())?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SecretStore;
    use crate::targets::{CommandOpts, CommandOutput, CommandRunner};
    use chrono::Utc;

    #[test]
    fn relative_time_days() {
        let ts = (Utc::now() - chrono::Duration::days(3)).to_rfc3339();
        assert_eq!(crate::ui::format_relative_time(&ts), "3d ago");
    }

    #[test]
    fn relative_time_hours() {
        let ts = (Utc::now() - chrono::Duration::hours(5)).to_rfc3339();
        assert_eq!(crate::ui::format_relative_time(&ts), "5h ago");
    }

    #[test]
    fn relative_time_minutes() {
        let ts = (Utc::now() - chrono::Duration::minutes(12)).to_rfc3339();
        assert_eq!(crate::ui::format_relative_time(&ts), "12m ago");
    }

    #[test]
    fn relative_time_just_now() {
        let ts = Utc::now().to_rfc3339();
        assert_eq!(crate::ui::format_relative_time(&ts), "just now");
    }

    #[test]
    fn relative_time_invalid() {
        assert_eq!(
            crate::ui::format_relative_time("not-a-timestamp"),
            "not-a-timestamp"
        );
    }

    struct OkRunner;
    impl CommandRunner for OkRunner {
        fn run(&self, _: &str, _: &[&str], _: CommandOpts) -> anyhow::Result<CommandOutput> {
            Ok(CommandOutput {
                success: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
    }

    #[test]
    fn remote_status_uses_env_scoped_version_for_stale() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: testapp
environments: [dev, prod]
remotes:
  1password:
    vault: Test
    item_pattern: "{project} - {Environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        SecretStore::load_or_create(dir.path()).unwrap();
        let config = Config::load(&path).unwrap();
        let store = SecretStore::open(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap(); // dev v1, prod v0 (implicit)

        let sync_index_path = dir.path().join(".esk/sync-index.json");
        let mut index = SyncIndex::new(&sync_index_path);
        index.record_success("1password", "dev", 0);
        index.save().unwrap();

        let dashboard = Dashboard::build(&config, Some("dev"), &OkRunner).unwrap();
        let dev = dashboard
            .remote_states
            .iter()
            .find(|ps| ps.name == "1password" && ps.env == "dev")
            .unwrap();
        assert!(matches!(
            dev.status,
            RemoteStatus::Stale {
                pushed: 0,
                local: 1
            }
        ));
    }

    #[test]
    fn remote_status_does_not_mark_other_env_stale() {
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
project: testapp
environments: [dev, prod]
remotes:
  1password:
    vault: Test
    item_pattern: "{project} - {Environment}"
"#;
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, yaml).unwrap();
        SecretStore::load_or_create(dir.path()).unwrap();
        let config = Config::load(&path).unwrap();
        let store = SecretStore::open(dir.path()).unwrap();
        store.set("KEY", "dev", "val").unwrap(); // global v1, prod env version remains 0

        let sync_index_path = dir.path().join(".esk/sync-index.json");
        let mut index = SyncIndex::new(&sync_index_path);
        index.record_success("1password", "prod", 0);
        index.save().unwrap();

        let dashboard = Dashboard::build(&config, None, &OkRunner).unwrap();
        let prod = dashboard
            .remote_states
            .iter()
            .find(|ps| ps.name == "1password" && ps.env == "prod")
            .unwrap();
        assert!(matches!(prod.status, RemoteStatus::Current { version: 0 }));
    }
}
