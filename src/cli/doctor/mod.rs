mod build;
mod render;
pub(crate) mod types;

use anyhow::Result;
use std::path::Path;

use crate::targets::{CommandRunner, RealCommandRunner};

use types::Report;

pub fn run(cwd: &Path) -> Result<()> {
    run_with_runner(cwd, &RealCommandRunner)
}

pub fn run_with_runner(cwd: &Path, runner: &dyn CommandRunner) -> Result<()> {
    let report = Report::build(cwd);
    report.render(runner)
}

#[cfg(test)]
mod tests {
    use super::types::*;
    use super::*;
    use crate::deploy_tracker::DeployIndex;
    use crate::store::SecretStore;
    use crate::sync_tracker::SyncIndex;
    use crate::targets::{CommandOpts, CommandOutput};
    use tempfile::TempDir;

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

    fn setup_healthy_project() -> TempDir {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
project: testapp
environments: [dev, prod]

apps:
  web:
    path: apps/web

targets:
  .env:
    pattern: "{app_path}/.env"

secrets:
  General:
    API_KEY:
      targets:
        .env: [web:dev, web:prod]
"#;
        std::fs::write(dir.path().join("esk.yaml"), yaml).unwrap();
        SecretStore::load_or_create(dir.path()).unwrap();

        // Create deploy index and sync index
        let deploy_idx = DeployIndex::new(&dir.path().join(".esk/deploy-index.json"));
        deploy_idx.save().unwrap();
        let sync_idx = SyncIndex::new(&dir.path().join(".esk/sync-index.json"));
        sync_idx.save().unwrap();

        // Add .esk/.gitignore entries
        let gitignore = crate::cli::init::ESK_GITIGNORE_ENTRIES.join("\n") + "\n";
        std::fs::write(dir.path().join(".esk/.gitignore"), gitignore).unwrap();

        dir
    }

    #[test]
    fn doctor_healthy_project() {
        let dir = setup_healthy_project();
        let report = Report::build(dir.path());

        assert!(report.project.as_deref() == Some("testapp"));

        // All structure checks should pass
        for check in &report.structure {
            assert_ne!(
                check.status,
                CheckStatus::Fail,
                "structure check '{}' failed: {}",
                check.label,
                check.detail
            );
        }

        // Config should be checked, not skipped
        assert!(matches!(report.config, Section::Checked(_)));

        // Store consistency should be checked
        assert!(matches!(report.store_consistency, Section::Checked(_)));

        // Secrets health should be checked
        assert!(matches!(report.secrets_health, Section::Checked(_)));
    }

    #[test]
    fn doctor_missing_esk_dir() {
        let dir = TempDir::new().unwrap();
        let report = Report::build(dir.path());

        assert!(report.project.is_none());

        // First structure check: .esk/ missing
        let esk_check = &report.structure[0];
        assert_eq!(esk_check.status, CheckStatus::Fail);
        assert!(esk_check.label.contains(".esk/"));

        // Sections should be skipped
        assert!(matches!(report.config, Section::Skipped(_)));
        assert!(matches!(report.store_consistency, Section::Skipped(_)));
        assert!(matches!(report.secrets_health, Section::Skipped(_)));

        // Should suggest esk init
        assert!(report.suggestions.iter().any(|s| s.command == "esk init"));
    }

    #[test]
    fn doctor_store_orphans() {
        let dir = setup_healthy_project();

        // Add a secret to store that isn't in config
        let store = SecretStore::open(dir.path()).unwrap();
        store.set("ORPHANED_KEY", "dev", "value").unwrap();

        let report = Report::build(dir.path());

        if let Section::Checked(checks) = &report.store_consistency {
            let orphan_check = checks.iter().find(|c| c.label == "Store orphans").unwrap();
            assert_eq!(orphan_check.status, CheckStatus::Warn);
            assert!(orphan_check
                .detail
                .contains("1 keys in store not in config"));
        } else {
            panic!("store_consistency should be Checked");
        }
    }

    #[test]
    fn doctor_missing_gitignore_entries() {
        let dir = setup_healthy_project();

        // Write partial .esk/.gitignore
        std::fs::write(dir.path().join(".esk/.gitignore"), "store.key\n").unwrap();

        let report = Report::build(dir.path());

        let gi_check = report
            .structure
            .iter()
            .find(|c| c.label == ".esk/.gitignore")
            .unwrap();
        assert_eq!(gi_check.status, CheckStatus::Warn);
        assert!(gi_check.detail.contains("missing"));
    }

    #[test]
    fn doctor_failed_deploys() {
        let dir = setup_healthy_project();

        // Set a value so it's in the store
        let store = SecretStore::open(dir.path()).unwrap();
        store.set("API_KEY", "dev", "sk-test").unwrap();

        // Record a failed deployment
        let deploy_path = dir.path().join(".esk/deploy-index.json");
        let mut index = DeployIndex::load(&deploy_path);
        index.record_failure(
            "API_KEY:.env:web:dev".to_string(),
            ".env:web:dev".to_string(),
            "hash".to_string(),
            "connection timeout".to_string(),
        );
        index.save().unwrap();

        let report = Report::build(dir.path());

        if let Section::Checked(checks) = &report.secrets_health {
            let failed_check = checks.iter().find(|c| c.label == "Failed deploys").unwrap();
            assert_eq!(failed_check.status, CheckStatus::Fail);
            assert!(failed_check
                .detail
                .contains("1 deployment(s) in failed state"));
        } else {
            panic!("secrets_health should be Checked");
        }

        // Should suggest esk deploy
        assert!(report.suggestions.iter().any(|s| s.command == "esk deploy"));
    }

    #[test]
    fn doctor_missing_required() {
        let dir = TempDir::new().unwrap();
        let yaml = r#"
project: testapp
environments: [dev]

apps:
  web:
    path: apps/web

targets:
  .env:
    pattern: "{app_path}/.env"

secrets:
  General:
    REQUIRED_KEY:
      required: true
      targets:
        .env: [web:dev]
"#;
        std::fs::write(dir.path().join("esk.yaml"), yaml).unwrap();
        SecretStore::load_or_create(dir.path()).unwrap();

        let deploy_idx = DeployIndex::new(&dir.path().join(".esk/deploy-index.json"));
        deploy_idx.save().unwrap();
        let sync_idx = SyncIndex::new(&dir.path().join(".esk/sync-index.json"));
        sync_idx.save().unwrap();

        let report = Report::build(dir.path());

        if let Section::Checked(checks) = &report.secrets_health {
            let req_check = checks
                .iter()
                .find(|c| c.label == "Required secrets")
                .unwrap();
            assert_eq!(req_check.status, CheckStatus::Warn);
            assert!(req_check.detail.contains("1 required secret(s) missing"));
        } else {
            panic!("secrets_health should be Checked");
        }
    }
}
