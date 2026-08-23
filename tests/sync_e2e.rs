mod helpers;

use std::collections::BTreeMap;

use esk::cli::sync::{self, SyncOptions};
use esk::config::{CloudFileFormat, Config};
use esk::reconcile::ConflictPreference;
use esk::remotes::cloud_file::CloudFileRemote;
use esk::remotes::SyncRemote;
use esk::store::{SecretStore, StorePayload};
use esk::sync_tracker::{SyncIndex, SyncStatus};
use helpers::{MockCommandRunner, TestProject};

fn sync_options() -> SyncOptions<'static> {
    SyncOptions {
        env: Some("dev"),
        only: None,
        dry_run: false,
        strict: true,
        force: false,
        auto_deploy: false,
        prefer: ConflictPreference::Local,
    }
}

fn remote_payload() -> StorePayload {
    StorePayload {
        secrets: BTreeMap::from([("REMOTE_KEY:dev".to_string(), "remote-value".to_string())]),
        version: 2,
        env_versions: BTreeMap::from([("dev".to_string(), 2)]),
        ..Default::default()
    }
}

fn write_config(project: &TestProject, remote_yaml: &str) -> Config {
    std::fs::write(
        project.root().join("esk.yaml"),
        format!("project: testapp\nenvironments: [dev]\nremotes:\n{remote_yaml}"),
    )
    .unwrap();
    SecretStore::load_or_create(project.root()).unwrap();
    project.config().unwrap()
}

#[test]
fn sync_cloud_file_cleartext_runs_pull_reconcile_and_push() {
    let project = TestProject::new("").unwrap();
    let cloud_dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &project,
        &format!(
            "  cloud:\n    type: cloud_file\n    path: {}\n    format: cleartext\n",
            cloud_dir.path().display()
        ),
    );
    let store = project.store().unwrap();
    store.set("LOCAL_KEY", "dev", "local-value").unwrap();

    let remote = CloudFileRemote::new(
        "cloud".to_string(),
        "testapp".to_string(),
        esk::config::CloudFileRemoteConfig {
            path: cloud_dir.path().to_string_lossy().into_owned(),
            format: CloudFileFormat::Cleartext,
        },
    );
    remote.push(&remote_payload(), &config, "dev").unwrap();

    sync::run_with_runner(&config, &sync_options(), &MockCommandRunner::new()).unwrap();

    let merged = project.store().unwrap().payload().unwrap();
    assert_eq!(
        merged.secrets.get("LOCAL_KEY:dev"),
        Some(&"local-value".to_string())
    );
    assert_eq!(
        merged.secrets.get("REMOTE_KEY:dev"),
        Some(&"remote-value".to_string())
    );
    let snapshot = remote.pull(&config, "dev").unwrap().unwrap();
    assert_eq!(snapshot.version, 3);
    assert_eq!(
        snapshot.secrets,
        merged
            .for_env("dev")
            .secrets
            .into_iter()
            .map(|(key, value)| { (format!("{key}:dev"), value) })
            .collect()
    );
}

#[test]
fn sync_cloud_file_encrypted_roundtrips_through_orchestration() {
    let project = TestProject::new("").unwrap();
    let cloud_dir = tempfile::tempdir().unwrap();
    let config = write_config(
        &project,
        &format!(
            "  cloud:\n    type: cloud_file\n    path: {}\n    format: encrypted\n",
            cloud_dir.path().display()
        ),
    );
    let store = project.store().unwrap();
    store.set("LOCAL_KEY", "dev", "local-value").unwrap();

    let remote = CloudFileRemote::new(
        "cloud".to_string(),
        "testapp".to_string(),
        esk::config::CloudFileRemoteConfig {
            path: cloud_dir.path().to_string_lossy().into_owned(),
            format: CloudFileFormat::Encrypted,
        },
    );
    remote.push(&remote_payload(), &config, "dev").unwrap();
    assert!(cloud_dir.path().join("secrets-dev.enc").is_file());

    sync::run_with_runner(&config, &sync_options(), &MockCommandRunner::new()).unwrap();

    let merged = project.store().unwrap().payload().unwrap();
    assert_eq!(
        merged.secrets.get("REMOTE_KEY:dev").unwrap(),
        "remote-value"
    );
    let snapshot = remote.pull(&config, "dev").unwrap().unwrap();
    assert_eq!(snapshot.version, 3);
}

#[test]
fn sync_pushes_merged_payload_to_multiple_cloud_remotes_and_tracks_each() {
    let project = TestProject::new("").unwrap();
    let cloud_one = tempfile::tempdir().unwrap();
    let cloud_two = tempfile::tempdir().unwrap();
    let config = write_config(
        &project,
        &format!(
            "  one:\n    type: cloud_file\n    path: {}\n    format: cleartext\n  two:\n    type: cloud_file\n    path: {}\n    format: cleartext\n",
            cloud_one.path().display(),
            cloud_two.path().display()
        ),
    );
    let store = project.store().unwrap();
    store.set("LOCAL_KEY", "dev", "local-value").unwrap();

    for (name, path) in [("one", cloud_one.path()), ("two", cloud_two.path())] {
        CloudFileRemote::new(
            name.to_string(),
            "testapp".to_string(),
            esk::config::CloudFileRemoteConfig {
                path: path.to_string_lossy().into_owned(),
                format: CloudFileFormat::Cleartext,
            },
        )
        .push(&remote_payload(), &config, "dev")
        .unwrap();
    }

    sync::run_with_runner(&config, &sync_options(), &MockCommandRunner::new()).unwrap();

    for path in [cloud_one.path(), cloud_two.path()] {
        let content = std::fs::read_to_string(path.join("secrets-dev.json")).unwrap();
        assert!(content.contains("LOCAL_KEY"));
        assert!(content.contains("REMOTE_KEY"));
    }
    let (index, _) = SyncIndex::load(&project.sync_index_path());
    for remote in ["one", "two"] {
        let record = index.records.get(&format!("{remote}:dev")).unwrap();
        assert_eq!(record.last_push_status, SyncStatus::Success);
        assert_eq!(record.pushed_version, 3);
    }
}
