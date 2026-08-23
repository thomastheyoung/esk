mod build;
mod render;
pub(crate) mod types;

use anyhow::Result;

use crate::config::Config;
use crate::targets::{CommandRunner, RealCommandRunner};

use types::Dashboard;

pub fn run(config: &Config, env: Option<&str>, all: bool) -> Result<()> {
    run_with_runner(config, env, all, &RealCommandRunner)
}

pub fn run_with_runner(
    config: &Config,
    env: Option<&str>,
    all: bool,
    runner: &dyn CommandRunner,
) -> Result<()> {
    validate_env_filter(config, env)?;
    let dashboard = Dashboard::build(config, env)?;
    dashboard.render(config, runner, all)
}

/// Emit the status dashboard as stable JSON without exposing secret values.
pub fn run_json(config: &Config, env: Option<&str>, all: bool) -> Result<()> {
    validate_env_filter(config, env)?;
    let dashboard = Dashboard::build(config, env)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&dashboard_json(&dashboard, all))?
    );
    Ok(())
}

fn validate_env_filter(config: &Config, env: Option<&str>) -> Result<()> {
    if let Some(env) = env {
        config.validate_env(env)?;
    }
    Ok(())
}

#[cfg(test)]
mod env_filter_tests {
    use super::*;

    #[test]
    fn human_and_json_status_reject_unknown_env_before_store_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(&path, "project: x\nenvironments: [dev]\n").unwrap();
        let config = Config::load(&path).unwrap();

        for error in [
            run(&config, Some("prdo"), false).unwrap_err(),
            run_json(&config, Some("prdo"), false).unwrap_err(),
        ] {
            assert!(error.to_string().contains("unknown environment 'prdo'"));
        }
    }
}

fn dashboard_json(dashboard: &Dashboard, all: bool) -> serde_json::Value {
    let entry = |e: &types::DeployEntry| {
        serde_json::json!({
            "key": e.key,
            "environment": e.env,
            "target": e.target,
            "error": e.error,
            "last_deployed_at": e.last_deployed_at,
        })
    };
    let remote = |r: &types::RemoteState| {
        let mut value = serde_json::json!({
            "name": r.name,
            "environment": r.env,
        });
        let status = match &r.status {
            types::RemoteStatus::Current { version } => {
                serde_json::json!({ "status": "current", "version": version })
            }
            types::RemoteStatus::Stale { pushed, local } => serde_json::json!({
                "status": "stale",
                "pushed_version": pushed,
                "local_version": local,
            }),
            types::RemoteStatus::Failed { version, error } => serde_json::json!({
                "status": "failed",
                "version": version,
                "error": error,
            }),
            types::RemoteStatus::NeverSynced => serde_json::json!({ "status": "never_synced" }),
        };
        value["status"] = status["status"].clone();
        if let Some(object) = status.as_object() {
            for (key, field) in object {
                if key != "status" {
                    value[key] = field.clone();
                }
            }
        }
        value
    };

    let output = serde_json::json!({
        "project": dashboard.project,
        "version": dashboard.version,
        "environment_filter": dashboard.filtered_env,
        "all": all,
        "environment_versions": dashboard.env_versions.iter().map(|(environment, version)| {
            serde_json::json!({ "environment": environment, "version": version })
        }).collect::<Vec<_>>(),
        "failed": dashboard.failed.iter().map(entry).collect::<Vec<_>>(),
        "pending": dashboard.pending.iter().map(entry).collect::<Vec<_>>(),
        "deployed": dashboard.deployed.iter().map(entry).collect::<Vec<_>>(),
        "unset": dashboard.unset.iter().map(entry).collect::<Vec<_>>(),
        "validation_warnings": dashboard.validation_warnings.iter().map(|w| serde_json::json!({
            "key": w.key, "environment": w.env, "message": w.message,
            "violations": w.violations,
        })).collect::<Vec<_>>(),
        "cross_field_violations": dashboard.cross_field_violations.iter().map(|v| serde_json::json!({
            "key": v.key(), "environment": v.env(), "message": v.message(),
            "code": v.code(), "references": v.references(),
        })).collect::<Vec<_>>(),
        "empty_values": dashboard.empty_values.iter().map(|w| serde_json::json!({
            "key": w.key, "environment": w.env, "kind": w.kind,
        })).collect::<Vec<_>>(),
        "missing_required": dashboard.missing_required.iter().map(|m| serde_json::json!({
            "key": m.key, "environment": m.env, "targets": m.targets,
        })).collect::<Vec<_>>(),
        "coverage_gaps": dashboard.coverage_gaps.iter().map(|g| serde_json::json!({
            "key": g.key, "missing_environments": g.missing_envs,
            "present_environments": g.present_envs,
        })).collect::<Vec<_>>(),
        "orphans": dashboard.orphans.iter().map(|o| serde_json::json!({
            "key": o.key, "environment": o.env,
        })).collect::<Vec<_>>(),
        "target_orphans": dashboard.target_orphans.iter().map(|o| serde_json::json!({
            "tracker_key": o.tracker_key, "key": o.key, "service": o.service,
            "app": o.app, "environment": o.env, "last_deployed_at": o.last_deployed_at,
        })).collect::<Vec<_>>(),
        "remote_states": dashboard.remote_states.iter().map(remote).collect::<Vec<_>>(),
        "next_steps": dashboard.next_steps.iter().map(|s| serde_json::json!({
            "command": s.command, "description": s.description,
        })).collect::<Vec<_>>(),
    });
    output
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::types::*;
    use crate::config::Config;
    use crate::store::SecretStore;
    use crate::sync_tracker::SyncIndex;
    use chrono::Utc;

    #[test]
    fn status_json_and_next_steps_do_not_disclose_validation_values() {
        let candidate = "candidate-sentinel";
        let allowed = "allowed-sentinel";
        let pattern = "^pattern-sentinel$";
        let predicate = "predicate-sentinel";
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("esk.yaml");
        std::fs::write(
            &path,
            format!(
                "project: demo\nenvironments: [dev]\nsecrets:\n  App:\n    TOKEN:\n      validate:\n        enum: [{allowed}]\n        pattern: '{pattern}'\n    REQUIRED:\n      validate:\n        required_if:\n          SWITCH: {predicate}\n    SWITCH: {{}}\n"
            ),
        )
        .unwrap();
        let store = SecretStore::load_or_create(dir.path()).unwrap();
        store.set("TOKEN", "dev", candidate).unwrap();
        store.set("SWITCH", "dev", predicate).unwrap();
        let config = Config::load(&path).unwrap();

        let dashboard = Dashboard::build(&config, Some("dev")).unwrap();
        let output = super::dashboard_json(&dashboard, false).to_string();
        for secret_material in [candidate, allowed, pattern, predicate] {
            assert!(!output.contains(secret_material), "{output}");
        }
        assert!(output.contains("\"violations\""));
        assert!(output.contains("\"required_if\""));
    }

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

        let dashboard = Dashboard::build(&config, Some("dev")).unwrap();
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
    fn group_entries_combines_targets() {
        let entries = vec![
            DeployEntry {
                key: "API_KEY".into(),
                env: "dev".into(),
                target: "cloudflare:web".into(),
                error: None,
                last_deployed_at: None,
            },
            DeployEntry {
                key: "API_KEY".into(),
                env: "dev".into(),
                target: "convex".into(),
                error: None,
                last_deployed_at: None,
            },
            DeployEntry {
                key: "API_KEY".into(),
                env: "dev".into(),
                target: "env:web".into(),
                error: None,
                last_deployed_at: None,
            },
        ];
        let groups = group_entries(&entries, TimestampPick::Oldest);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].targets,
            vec!["cloudflare:web", "convex", "env:web"]
        );
        assert_eq!(groups[0].freshness, GroupedFreshness::NeverDeployed);
    }

    #[test]
    fn group_entries_picks_oldest_for_pending() {
        let entries = vec![
            DeployEntry {
                key: "K".into(),
                env: "dev".into(),
                target: "a".into(),
                error: None,
                last_deployed_at: Some("2025-01-03T00:00:00Z".into()),
            },
            DeployEntry {
                key: "K".into(),
                env: "dev".into(),
                target: "b".into(),
                error: None,
                last_deployed_at: Some("2025-01-01T00:00:00Z".into()),
            },
        ];
        let groups = group_entries(&entries, TimestampPick::Oldest);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].freshness,
            GroupedFreshness::Timestamp("2025-01-01T00:00:00Z".into())
        );
    }

    #[test]
    fn group_entries_picks_newest_for_deployed() {
        let entries = vec![
            DeployEntry {
                key: "K".into(),
                env: "dev".into(),
                target: "a".into(),
                error: None,
                last_deployed_at: Some("2025-01-01T00:00:00Z".into()),
            },
            DeployEntry {
                key: "K".into(),
                env: "dev".into(),
                target: "b".into(),
                error: None,
                last_deployed_at: Some("2025-01-03T00:00:00Z".into()),
            },
        ];
        let groups = group_entries(&entries, TimestampPick::Newest);
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].freshness,
            GroupedFreshness::Timestamp("2025-01-03T00:00:00Z".into())
        );
    }

    #[test]
    fn group_entries_never_deployed_wins() {
        let entries = vec![
            DeployEntry {
                key: "K".into(),
                env: "dev".into(),
                target: "a".into(),
                error: None,
                last_deployed_at: Some("2025-01-01T00:00:00Z".into()),
            },
            DeployEntry {
                key: "K".into(),
                env: "dev".into(),
                target: "b".into(),
                error: None,
                last_deployed_at: None,
            },
        ];
        let groups = group_entries(&entries, TimestampPick::Oldest);
        assert_eq!(groups[0].freshness, GroupedFreshness::NeverDeployed);
    }

    #[test]
    fn group_entries_separate_envs() {
        let entries = vec![
            DeployEntry {
                key: "K".into(),
                env: "dev".into(),
                target: "a".into(),
                error: None,
                last_deployed_at: None,
            },
            DeployEntry {
                key: "K".into(),
                env: "prod".into(),
                target: "a".into(),
                error: None,
                last_deployed_at: None,
            },
        ];
        let groups = group_entries(&entries, TimestampPick::Oldest);
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn truncation_footer_none_within_limit() {
        assert!(crate::ui::truncation_footer(5, 5).is_none());
        assert!(crate::ui::truncation_footer(3, 5).is_none());
    }

    #[test]
    fn truncation_footer_some_over_limit() {
        let footer = crate::ui::truncation_footer(12, 5).unwrap();
        let plain = console::strip_ansi_codes(&footer);
        assert!(plain.contains("7 more"));
        assert!(plain.contains("--all to show"));
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

        let dashboard = Dashboard::build(&config, None).unwrap();
        let prod = dashboard
            .remote_states
            .iter()
            .find(|ps| ps.name == "1password" && ps.env == "prod")
            .unwrap();
        assert!(matches!(prod.status, RemoteStatus::Current { version: 0 }));
    }
}
