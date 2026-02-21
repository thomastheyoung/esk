mod helpers;

use helpers::*;
use lockbox::cli;
use lockbox::tracker::SyncIndex;

// === init ===

#[test]
fn init_creates_all_files() {
    let dir = tempfile::tempdir().unwrap();
    cli::init::run(dir.path()).unwrap();
    assert!(dir.path().join("lockbox.yaml").is_file());
    assert!(dir.path().join(".secrets.enc").is_file());
    assert!(dir.path().join(".secrets.key").is_file());
    assert!(dir.path().join(".sync-index.json").is_file());
}

#[test]
fn init_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    cli::init::run(dir.path()).unwrap();
    let key1 = std::fs::read_to_string(dir.path().join(".secrets.key")).unwrap();
    cli::init::run(dir.path()).unwrap();
    let key2 = std::fs::read_to_string(dir.path().join(".secrets.key")).unwrap();
    assert_eq!(key1, key2); // Key not overwritten
}

#[test]
fn init_gitignore_warning() {
    let dir = tempfile::tempdir().unwrap();
    // Create .gitignore without .secrets.key
    std::fs::write(dir.path().join(".gitignore"), "node_modules\n").unwrap();
    // Should not error (just warns)
    cli::init::run(dir.path()).unwrap();
}

// === get ===

#[test]
fn get_unknown_env_errors() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::get::run(&config, "KEY", "staging").unwrap_err();
    assert!(err.to_string().contains("unknown environment"));
}

#[test]
fn get_missing_value() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::get::run(&config, "NONEXISTENT", "dev").unwrap_err();
    assert!(err.to_string().contains("no value"));
}

#[test]
fn get_returns_value() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_KEY", "dev", "my_value").unwrap();

    // get::run prints to stdout — we just verify it doesn't error
    cli::get::run(&config, "MY_KEY", "dev").unwrap();
}

// === set ===

#[test]
fn set_unknown_env_errors() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::set::run(&config, "KEY", "staging", Some("val"), true).unwrap_err();
    assert!(err.to_string().contains("unknown environment"));
}

#[test]
fn set_with_value_flag() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    cli::set::run(&config, "TEST_KEY", "dev", Some("test_value"), true).unwrap();

    let store = project.store().unwrap();
    assert_eq!(
        store.get("TEST_KEY", "dev").unwrap(),
        Some("test_value".to_string())
    );
}

#[test]
fn set_warns_undeclared_key() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    // KEY not in config — should warn but succeed
    cli::set::run(&config, "UNDECLARED", "dev", Some("val"), true).unwrap();
    let store = project.store().unwrap();
    assert_eq!(
        store.get("UNDECLARED", "dev").unwrap(),
        Some("val".to_string())
    );
}

#[test]
fn set_no_sync_flag() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    cli::set::run(&config, "KEY", "dev", Some("val"), true).unwrap();
    // With no_sync=true, no sync should have happened — just verify set worked
    let store = project.store().unwrap();
    assert_eq!(store.get("KEY", "dev").unwrap(), Some("val".to_string()));
}

// === list ===

#[test]
fn list_empty_store() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    cli::list::run(&config, None).unwrap(); // Should print help message
}

#[test]
fn list_with_env_filter() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "dv").unwrap();
    store.set("MY_SECRET", "prod", "pv").unwrap();

    // Filter by dev — should work without error
    cli::list::run(&config, Some("dev")).unwrap();
}

#[test]
fn list_uncategorized_secrets() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    // Set a secret that's not in config
    store.set("ORPHAN_KEY", "dev", "val").unwrap();

    cli::list::run(&config, None).unwrap(); // Should show "Uncategorized"
}

// === push ===

#[test]
fn push_unknown_env_errors() {
    let project = TestProject::with_store(PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::push::run(&config, "staging", None).unwrap_err();
    assert!(err.to_string().contains("unknown environment"));
}

#[test]
fn push_no_plugins() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::push::run(&config, "dev", None).unwrap_err();
    assert!(err.to_string().contains("no plugins configured"));
}

// === pull ===

#[test]
fn pull_unknown_env_errors() {
    let project = TestProject::with_store(PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::pull::run(&config, "staging", None, false).unwrap_err();
    assert!(err.to_string().contains("unknown environment"));
}

#[test]
fn pull_no_plugins() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::pull::run(&config, "dev", None, false).unwrap_err();
    assert!(err.to_string().contains("no plugins configured"));
}

// === sync ===

#[test]
fn sync_env_filter() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "dv").unwrap();
    store.set("MY_SECRET", "prod", "pv").unwrap();

    // Sync only dev
    cli::sync::run(&config, Some("dev"), false, false, false).unwrap();

    // dev env file should exist
    assert!(project.root().join("apps/web/.env.local").is_file());
    // prod env file should NOT exist (only synced dev)
    assert!(!project
        .root()
        .join("apps/web/.env.production.local")
        .is_file());
}

#[test]
fn sync_dry_run_no_side_effects() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "dv").unwrap();
    store.set("OTHER_SECRET", "dev", "ov").unwrap();

    cli::sync::run(&config, None, false, true, false).unwrap();

    // No env file should be created
    assert!(!project.root().join("apps/web/.env.local").is_file());
    // No sync index should exist
    assert!(!project.sync_index_path().is_file());
}

#[test]
fn sync_force_resyncs_unchanged() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();
    store.set("OTHER_SECRET", "dev", "val2").unwrap();

    // First sync
    cli::sync::run(&config, Some("dev"), false, false, false).unwrap();
    let mtime1 = std::fs::metadata(project.root().join("apps/web/.env.local"))
        .unwrap()
        .modified()
        .unwrap();

    // Small sleep to ensure mtime differs
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Force sync — should regenerate even though hashes match
    cli::sync::run(&config, Some("dev"), true, false, false).unwrap();
    let mtime2 = std::fs::metadata(project.root().join("apps/web/.env.local"))
        .unwrap()
        .modified()
        .unwrap();

    assert!(mtime2 > mtime1);
}

#[test]
fn sync_skips_plugin_targets() {
    // Secrets with targets that reference a non-adapter name should be skipped
    // Since onepassword is now a plugin, not an adapter, we just verify sync works
    // with a config that has both adapter and plugin targets
    let yaml = r#"
project: testapp
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env{env_suffix}.local"
    env_suffix:
      dev: ""
secrets:
  G:
    MY_SECRET:
      targets:
        env: [web:dev]
"#;
    let project = TestProject::with_store(yaml).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();

    // Sync should succeed — only hitting env adapter
    cli::sync::run(&config, None, false, false, false).unwrap();
    assert!(project.root().join("apps/web/.env.local").is_file());
}

#[test]
fn sync_skips_no_value_secrets() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    // Don't set any values — sync should still work, just skip everything
    cli::sync::run(&config, None, false, false, true).unwrap();
}

#[test]
fn sync_failure_count_causes_error() {
    // This tests uses env adapter with an app that can't write
    let yaml = r#"
project: x
environments: [dev]
apps:
  web:
    path: /nonexistent/path/that/wont/work
adapters:
  env:
    pattern: "{app_path}/.env"
secrets:
  G:
    KEY:
      targets:
        env: [web:dev]
"#;
    let project = TestProject::with_store(yaml).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("KEY", "dev", "val").unwrap();

    let err = cli::sync::run(&config, None, false, false, false).unwrap_err();
    assert!(err.to_string().contains("failed"));
}

#[test]
fn sync_env_dirty_pair_regens_all() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val1").unwrap();
    store.set("OTHER_SECRET", "dev", "val2").unwrap();

    // First sync
    cli::sync::run(&config, Some("dev"), false, false, false).unwrap();

    // Change only MY_SECRET
    store.set("MY_SECRET", "dev", "val1_changed").unwrap();

    // Sync again — should regenerate entire file including OTHER_SECRET
    cli::sync::run(&config, Some("dev"), false, false, false).unwrap();

    let content = std::fs::read_to_string(project.root().join("apps/web/.env.local")).unwrap();
    assert!(content.contains("MY_SECRET=val1_changed"));
    assert!(content.contains("OTHER_SECRET=val2"));
}

// === status ===

#[test]
fn status_shows_all_states() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();

    // Set some values
    store.set("MY_SECRET", "dev", "val").unwrap();
    // Don't set OTHER_SECRET → "no value"

    // Sync to create "synced" state
    cli::sync::run(&config, Some("dev"), false, false, false).unwrap();

    // Change MY_SECRET → "pending" state for dev
    store.set("MY_SECRET", "dev", "changed").unwrap();

    // Status should work without error
    cli::status::run(&config, None).unwrap();
}

#[test]
fn status_env_filter() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();

    cli::status::run(&config, Some("dev")).unwrap();
}

#[test]
fn sync_records_to_tracker() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();
    store.set("OTHER_SECRET", "dev", "val2").unwrap();

    cli::sync::run(&config, Some("dev"), false, false, false).unwrap();

    let index = SyncIndex::load(&project.sync_index_path()).unwrap();
    assert!(!index.records.is_empty());
    // Should have records for each synced secret
    let keys: Vec<&String> = index.records.keys().collect();
    assert!(keys.iter().any(|k| k.contains("MY_SECRET")));
    assert!(keys.iter().any(|k| k.contains("OTHER_SECRET")));
}

// === cloud_file plugin integration ===

#[test]
fn cloud_file_push_pull_cleartext() {
    use lockbox::config::{CloudFileFormat, CloudFilePluginConfig};
    use lockbox::plugins::cloud_file::CloudFilePlugin;
    use lockbox::plugins::StoragePlugin;

    let project_dir = tempfile::tempdir().unwrap();
    let cloud_dir = tempfile::tempdir().unwrap();

    let yaml = "project: testapp\nenvironments: [dev]";
    std::fs::write(project_dir.path().join("lockbox.yaml"), yaml).unwrap();
    lockbox::store::SecretStore::load_or_create(project_dir.path()).unwrap();
    let config = lockbox::config::Config::load(&project_dir.path().join("lockbox.yaml")).unwrap();

    let store = lockbox::store::SecretStore::open(&config.root).unwrap();
    store.set("KEY", "dev", "val123").unwrap();
    let payload = store.payload().unwrap();

    let plugin = CloudFilePlugin::new(
        "test_cloud".to_string(),
        CloudFilePluginConfig {
            path: cloud_dir.path().to_string_lossy().to_string(),
            format: CloudFileFormat::Cleartext,
        },
    );

    plugin.push(&payload, &config, "dev").unwrap();
    assert!(cloud_dir.path().join("secrets.json").is_file());

    let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
    assert_eq!(version, 1);
    assert_eq!(secrets.get("KEY:dev").unwrap(), "val123");
}

#[test]
fn cloud_file_push_pull_encrypted() {
    use lockbox::config::{CloudFileFormat, CloudFilePluginConfig};
    use lockbox::plugins::cloud_file::CloudFilePlugin;
    use lockbox::plugins::StoragePlugin;

    let project_dir = tempfile::tempdir().unwrap();
    let cloud_dir = tempfile::tempdir().unwrap();

    let yaml = "project: testapp\nenvironments: [dev]";
    std::fs::write(project_dir.path().join("lockbox.yaml"), yaml).unwrap();
    lockbox::store::SecretStore::load_or_create(project_dir.path()).unwrap();
    let config = lockbox::config::Config::load(&project_dir.path().join("lockbox.yaml")).unwrap();

    let store = lockbox::store::SecretStore::open(&config.root).unwrap();
    store.set("SECRET", "dev", "encrypted_val").unwrap();
    let payload = store.payload().unwrap();

    let plugin = CloudFilePlugin::new(
        "test_enc".to_string(),
        CloudFilePluginConfig {
            path: cloud_dir.path().to_string_lossy().to_string(),
            format: CloudFileFormat::Encrypted,
        },
    );

    plugin.push(&payload, &config, "dev").unwrap();
    assert!(cloud_dir.path().join("secrets.enc").is_file());

    let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
    assert_eq!(version, 1);
    assert_eq!(secrets.get("SECRET:dev").unwrap(), "encrypted_val");
}

#[test]
fn push_only_flag() {
    // Test that --only filters to a specific plugin
    // We can't easily test with real plugins in integration, but we can verify
    // the error when specifying an unknown plugin name
    let yaml = r#"
project: testapp
environments: [dev]
plugins:
  onepassword:
    vault: Test
    item_pattern: test
"#;
    let project = TestProject::with_store(yaml).unwrap();
    let config = project.config().unwrap();
    let err = cli::push::run(&config, "dev", Some("nonexistent")).unwrap_err();
    assert!(err.to_string().contains("unknown plugin"));
}

#[test]
fn pull_only_flag() {
    let yaml = r#"
project: testapp
environments: [dev]
plugins:
  onepassword:
    vault: Test
    item_pattern: test
"#;
    let project = TestProject::with_store(yaml).unwrap();
    let config = project.config().unwrap();
    let err = cli::pull::run(&config, "dev", Some("nonexistent"), false).unwrap_err();
    assert!(err.to_string().contains("unknown plugin"));
}
