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
    let dashboard = Dashboard::build(config, env)?;
    dashboard.render(config, runner, all)
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
