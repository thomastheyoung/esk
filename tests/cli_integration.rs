mod helpers;

use esk::cli;
use esk::plugin_tracker::PluginIndex;
use esk::tracker::SyncIndex;
use helpers::*;
use serde_json::json;

// === init ===

#[test]
fn init_creates_all_files() {
    let dir = tempfile::tempdir().unwrap();
    cli::init::run(dir.path()).unwrap();
    assert!(dir.path().join("esk.yaml").is_file());
    assert!(dir.path().join(".esk/store.enc").is_file());
    assert!(dir.path().join(".esk/store.key").is_file());
    assert!(dir.path().join(".esk/sync-index.json").is_file());
    assert!(dir.path().join(".esk/plugin-index.json").is_file());
}

#[test]
fn init_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    cli::init::run(dir.path()).unwrap();
    let key1 = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
    cli::init::run(dir.path()).unwrap();
    let key2 = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
    assert_eq!(key1, key2); // Key not overwritten
}

#[test]
fn init_gitignore_warning() {
    let dir = tempfile::tempdir().unwrap();
    // Create .gitignore without .esk/store.key
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
    let err = cli::set::run(&config, "KEY", "staging", Some("val"), None, true, false).unwrap_err();
    assert!(err.to_string().contains("unknown environment"));
}

#[test]
fn set_with_value_flag() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    cli::set::run(
        &config,
        "TEST_KEY",
        "dev",
        Some("test_value"),
        None,
        true,
        false,
    )
    .unwrap();

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
    // KEY not in config, no --group, non-TTY — should warn but succeed
    cli::set::run(&config, "UNDECLARED", "dev", Some("val"), None, true, false).unwrap();
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
    cli::set::run(&config, "KEY", "dev", Some("val"), None, true, false).unwrap();
    // With no_sync=true, no sync should have happened — just verify set worked
    let store = project.store().unwrap();
    assert_eq!(store.get("KEY", "dev").unwrap(), Some("val".to_string()));
}

#[test]
fn set_with_group_flag_adds_to_config() {
    let yaml = "project: testapp\nenvironments: [dev]\nsecrets:\n  Stripe:\n    EXISTING: {}\n";
    let project = TestProject::with_store(yaml).unwrap();
    let config = project.config().unwrap();
    cli::set::run(
        &config,
        "NEW_KEY",
        "dev",
        Some("val"),
        Some("Stripe"),
        true,
        false,
    )
    .unwrap();

    // Key should be in the store
    let store = project.store().unwrap();
    assert_eq!(
        store.get("NEW_KEY", "dev").unwrap(),
        Some("val".to_string())
    );

    // Key should now appear in config under Stripe
    let reloaded = project.config().unwrap();
    assert!(reloaded.find_secret("NEW_KEY").is_some());
    let (vendor, _) = reloaded.find_secret("NEW_KEY").unwrap();
    assert_eq!(vendor, "Stripe");
}

#[test]
fn set_with_group_flag_creates_new_group() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    cli::set::run(
        &config,
        "API_KEY",
        "dev",
        Some("val"),
        Some("NewVendor"),
        true,
        false,
    )
    .unwrap();

    let reloaded = project.config().unwrap();
    let (vendor, _) = reloaded.find_secret("API_KEY").unwrap();
    assert_eq!(vendor, "NewVendor");
}

#[test]
fn set_with_group_flag_existing_key_no_duplicate() {
    let yaml = "project: testapp\nenvironments: [dev]\nsecrets:\n  Stripe:\n    SK: {}\n";
    let project = TestProject::with_store(yaml).unwrap();
    let config = project.config().unwrap();

    // SK already exists in config — --group should be a no-op for config registration
    cli::set::run(
        &config,
        "SK",
        "dev",
        Some("val"),
        Some("Stripe"),
        true,
        false,
    )
    .unwrap();

    // Verify no duplicate: SK should appear exactly once
    let content = std::fs::read_to_string(project.root().join("esk.yaml")).unwrap();
    assert_eq!(content.matches("    SK:").count(), 1);
}

// === delete ===

#[test]
fn delete_removes_secret() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();
    cli::delete::run_with_runner(
        &config,
        "MY_SECRET",
        "dev",
        true,
        false,
        &MockCommandRunner::new(),
    )
    .unwrap();
    assert!(store.get("MY_SECRET", "dev").unwrap().is_none());
}

#[test]
fn delete_unknown_env_errors() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::delete::run_with_runner(
        &config,
        "MY_SECRET",
        "staging",
        true,
        false,
        &MockCommandRunner::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown environment"));
}

#[test]
fn delete_nonexistent_secret_errors() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::delete::run_with_runner(
        &config,
        "MY_SECRET",
        "dev",
        true,
        false,
        &MockCommandRunner::new(),
    )
    .unwrap_err();
    assert!(err.to_string().contains("no value for environment"));
}

#[test]
fn delete_auto_syncs_env_file() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val1").unwrap();
    store.set("OTHER_SECRET", "dev", "val2").unwrap();

    // Sync first to write both secrets
    let runner = MockCommandRunner::new();
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let env_path = project.root().join("apps/web/.env.local");
    let contents = std::fs::read_to_string(&env_path).unwrap();
    assert!(contents.contains("MY_SECRET=val1"));
    assert!(contents.contains("OTHER_SECRET=val2"));

    // Delete MY_SECRET
    cli::delete::run_with_runner(&config, "MY_SECRET", "dev", false, false, &runner).unwrap();

    // Env file should no longer contain MY_SECRET
    let contents = std::fs::read_to_string(&env_path).unwrap();
    assert!(!contents.contains("MY_SECRET"));
    assert!(contents.contains("OTHER_SECRET=val2"));
}

#[test]
fn delete_creates_tombstone() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();
    cli::delete::run_with_runner(
        &config,
        "MY_SECRET",
        "dev",
        true,
        false,
        &MockCommandRunner::new(),
    )
    .unwrap();

    let payload = store.payload().unwrap();
    assert!(payload.tombstones.contains_key("MY_SECRET:dev"));
}

#[test]
fn set_strict_plugin_failure_blocks_adapter_sync() {
    let yaml = r#"
project: testapp
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env.local"
plugins:
  onepassword:
    vault: V
    item_pattern: test
secrets:
  General:
    MY_SECRET:
      targets:
        env: [web:dev]
"#;
    let project = TestProject::with_store(yaml).unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let config = project.config().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
    runner.push_failure(b"push failed"); // op item get fails => push fails

    let err = cli::set::run_with_runner(
        &config,
        "MY_SECRET",
        "dev",
        Some("val"),
        None,
        false,
        true,
        &runner,
    )
    .unwrap_err();
    assert!(err.to_string().contains("--strict"));
    assert!(err.to_string().contains("Adapter deploy skipped"));

    // Env file should NOT have been written
    let env_path = project.root().join("apps/web/.env.local");
    assert!(!env_path.exists());
}

#[test]
fn delete_strict_plugin_failure_blocks_adapter_sync() {
    let yaml = r#"
project: testapp
environments: [dev]
apps:
  web:
    path: apps/web
adapters:
  env:
    pattern: "{app_path}/.env.local"
plugins:
  onepassword:
    vault: V
    item_pattern: test
secrets:
  General:
    MY_SECRET:
      targets:
        env: [web:dev]
    OTHER:
      targets:
        env: [web:dev]
"#;
    let project = TestProject::with_store(yaml).unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();
    store.set("OTHER", "dev", "other_val").unwrap();

    // First sync to write the env file
    cli::deploy::run_with_runner(
        &config,
        Some("dev"),
        false,
        false,
        false,
        &MockCommandRunner::new(),
    )
    .unwrap();
    let env_path = project.root().join("apps/web/.env.local");
    assert!(env_path.exists());
    let before = std::fs::read_to_string(&env_path).unwrap();
    assert!(before.contains("MY_SECRET=val"));

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
    runner.push_failure(b"push failed"); // op item get fails => push fails

    let err = cli::delete::run_with_runner(&config, "MY_SECRET", "dev", false, true, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("--strict"));
    assert!(err.to_string().contains("Adapter deploy skipped"));

    // Env file should still contain MY_SECRET (sync was skipped)
    let after = std::fs::read_to_string(&env_path).unwrap();
    assert!(after.contains("MY_SECRET=val"));
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

// === plugin-sync ===

#[test]
fn plugin_sync_unknown_env_errors() {
    let project = TestProject::with_store(PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::sync::run(&config, "staging", None, false, false, false, false).unwrap_err();
    assert!(err.to_string().contains("unknown environment"));
}

#[test]
fn plugin_sync_no_plugins() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let err = cli::sync::run(&config, "dev", None, false, false, false, false).unwrap_err();
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
    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();

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

    cli::deploy::run(&config, None, false, true, false).unwrap();

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
    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();
    let mtime1 = std::fs::metadata(project.root().join("apps/web/.env.local"))
        .unwrap()
        .modified()
        .unwrap();

    // Small sleep to ensure mtime differs
    std::thread::sleep(std::time::Duration::from_millis(50));

    // Force sync — should regenerate even though hashes match
    cli::deploy::run(&config, Some("dev"), true, false, false).unwrap();
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
    cli::deploy::run(&config, None, false, false, false).unwrap();
    assert!(project.root().join("apps/web/.env.local").is_file());
}

#[test]
fn sync_skips_no_value_secrets() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    // Don't set any values — sync should still work, just skip everything
    cli::deploy::run(&config, None, false, false, true).unwrap();
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

    let err = cli::deploy::run(&config, None, false, false, false).unwrap_err();
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
    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();

    // Change only MY_SECRET
    store.set("MY_SECRET", "dev", "val1_changed").unwrap();

    // Sync again — should regenerate entire file including OTHER_SECRET
    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();

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
    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();

    // Change MY_SECRET → "pending" state for dev
    store.set("MY_SECRET", "dev", "changed").unwrap();

    // Status should work without error
    cli::status::run(&config, None, false).unwrap();
}

#[test]
fn status_env_filter() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();

    cli::status::run(&config, Some("dev"), false).unwrap();
}

#[test]
fn sync_records_to_tracker() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("MY_SECRET", "dev", "val").unwrap();
    store.set("OTHER_SECRET", "dev", "val2").unwrap();

    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(!index.records.is_empty());
    // Should have records for each synced secret
    let keys: Vec<&String> = index.records.keys().collect();
    assert!(keys.iter().any(|k| k.contains("MY_SECRET")));
    assert!(keys.iter().any(|k| k.contains("OTHER_SECRET")));
}

// === cloud_file plugin integration ===

#[test]
fn cloud_file_push_pull_cleartext() {
    use esk::config::{CloudFileFormat, CloudFilePluginConfig};
    use esk::plugins::cloud_file::CloudFilePlugin;
    use esk::plugins::StoragePlugin;

    let project_dir = tempfile::tempdir().unwrap();
    let cloud_dir = tempfile::tempdir().unwrap();

    let yaml = "project: testapp\nenvironments: [dev]";
    std::fs::write(project_dir.path().join("esk.yaml"), yaml).unwrap();
    esk::store::SecretStore::load_or_create(project_dir.path()).unwrap();
    let config = esk::config::Config::load(&project_dir.path().join("esk.yaml")).unwrap();

    let store = esk::store::SecretStore::open(&config.root).unwrap();
    store.set("KEY", "dev", "val123").unwrap();
    let payload = store.payload().unwrap();

    let plugin = CloudFilePlugin::new(
        "test_cloud".to_string(),
        "testapp".to_string(),
        CloudFilePluginConfig {
            path: cloud_dir.path().to_string_lossy().to_string(),
            format: CloudFileFormat::Cleartext,
        },
    );

    plugin.push(&payload, &config, "dev").unwrap();
    assert!(cloud_dir.path().join("secrets-dev.json").is_file());

    let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
    assert_eq!(version, 1);
    assert_eq!(secrets.get("KEY:dev").unwrap(), "val123");
}

#[test]
fn cloud_file_push_pull_encrypted() {
    use esk::config::{CloudFileFormat, CloudFilePluginConfig};
    use esk::plugins::cloud_file::CloudFilePlugin;
    use esk::plugins::StoragePlugin;

    let project_dir = tempfile::tempdir().unwrap();
    let cloud_dir = tempfile::tempdir().unwrap();

    let yaml = "project: testapp\nenvironments: [dev]";
    std::fs::write(project_dir.path().join("esk.yaml"), yaml).unwrap();
    esk::store::SecretStore::load_or_create(project_dir.path()).unwrap();
    let config = esk::config::Config::load(&project_dir.path().join("esk.yaml")).unwrap();

    let store = esk::store::SecretStore::open(&config.root).unwrap();
    store.set("SECRET", "dev", "encrypted_val").unwrap();
    let payload = store.payload().unwrap();

    let plugin = CloudFilePlugin::new(
        "test_enc".to_string(),
        "testapp".to_string(),
        CloudFilePluginConfig {
            path: cloud_dir.path().to_string_lossy().to_string(),
            format: CloudFileFormat::Encrypted,
        },
    );

    plugin.push(&payload, &config, "dev").unwrap();
    assert!(cloud_dir.path().join("secrets-dev.enc").is_file());

    let (secrets, version) = plugin.pull(&config, "dev").unwrap().unwrap();
    assert_eq!(version, 1);
    assert_eq!(secrets.get("SECRET:dev").unwrap(), "encrypted_val");
}

#[test]
fn plugin_sync_only_flag() {
    // Test that --only filters to a specific plugin
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
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
    let err = cli::sync::run_with_runner(
        &config,
        "dev",
        Some("nonexistent"),
        false,
        false,
        false,
        false,
        &runner,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown plugin"));
}

// === cloudflare adapter integration ===

#[test]
fn sync_cloudflare_calls_wrangler() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test_123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "wrangler");
    assert_eq!(calls[2].args, vec!["secret", "put", "STRIPE_KEY"]);
    assert_eq!(
        calls[2].cwd.as_ref().unwrap(),
        &project.root().join("apps/web")
    );
    assert_eq!(calls[2].stdin.as_ref().unwrap(), b"sk_test_123");
}

#[test]
fn sync_cloudflare_prod_env_flags() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "prod", "sk_live_456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2].args,
        vec!["secret", "put", "STRIPE_KEY", "--env", "production"]
    );
}

#[test]
fn sync_cloudflare_records_tracker() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(!index.records.is_empty());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("STRIPE_KEY") && k.contains("cloudflare")));
}

#[test]
fn sync_cloudflare_failure_tracked() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_failure(b"auth error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("cloudflare"))
        .unwrap();
    assert!(record.last_error.is_some());
}

#[test]
fn sync_cloudflare_multiple_secrets() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    store.set("STRIPE_WEBHOOK", "dev", "whsec_test").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b"");
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 4);
    // Skip preflight calls (index 0-1), check sync calls
    let keys: Vec<&str> = calls[2..].iter().map(|c| c.args[2].as_str()).collect();
    assert!(keys.contains(&"STRIPE_KEY"));
    assert!(keys.contains(&"STRIPE_WEBHOOK"));
}

#[test]
fn sync_cloudflare_skip_unchanged() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b"");

    // First sync
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 3); // preflight (2) + sync

    // Second sync — same value, should skip (only preflight calls)
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 2); // preflight only
}

// === convex adapter integration ===

#[test]
fn sync_convex_calls_npx() {
    let project = TestProject::with_store(CONVEX_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/api")).unwrap();
    let store = project.store().unwrap();
    store
        .set("CONVEX_URL", "dev", "https://dev.convex.cloud")
        .unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: npx --version
    runner.push_success(b"", b""); // preflight: convex env list
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "npx");
    assert_eq!(
        calls[2].args,
        vec![
            "convex",
            "env",
            "set",
            "CONVEX_URL",
            "https://dev.convex.cloud"
        ]
    );
    assert_eq!(
        calls[2].cwd.as_ref().unwrap(),
        &project.root().join("apps/api")
    );
}

#[test]
fn sync_convex_prod_env_flags() {
    let project = TestProject::with_store(CONVEX_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/api")).unwrap();
    let store = project.store().unwrap();
    store
        .set("CONVEX_URL", "prod", "https://prod.convex.cloud")
        .unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: npx --version
    runner.push_success(b"", b""); // preflight: convex env list
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls[2].args,
        vec![
            "convex",
            "env",
            "set",
            "CONVEX_URL",
            "https://prod.convex.cloud",
            "--prod"
        ]
    );
}

#[test]
fn sync_convex_reads_deployment_source() {
    let project = TestProject::with_store(CONVEX_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/api")).unwrap();
    std::fs::write(
        project.root().join("apps/api/.env.local"),
        "CONVEX_DEPLOYMENT=dev:my-deploy-abc\n",
    )
    .unwrap();
    let store = project.store().unwrap();
    store
        .set("CONVEX_URL", "dev", "https://dev.convex.cloud")
        .unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: npx --version
    runner.push_success(b"", b""); // preflight: convex env list
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[2].env.contains(&(
        "CONVEX_DEPLOYMENT".to_string(),
        "dev:my-deploy-abc".to_string()
    )));
}

#[test]
fn sync_convex_failure_tracked() {
    let project = TestProject::with_store(CONVEX_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/api")).unwrap();
    let store = project.store().unwrap();
    store
        .set("CONVEX_URL", "dev", "https://dev.convex.cloud")
        .unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: npx --version
    runner.push_success(b"", b""); // preflight: convex env list
    runner.push_failure(b"deploy error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("convex"))
        .unwrap();
    assert!(record.last_error.is_some());
}

// === onepassword plugin integration ===

#[test]
fn push_onepassword_creates_item() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test_999").unwrap();
    let payload = store.payload().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
                                   // op item get → not found
    runner.push_failure(b"isn't an item in vault");
    // op item create → success
    runner.push_success(b"", b"");

    let plugins = esk::plugins::build_plugins(&config, &runner);
    let mut plugin_index = PluginIndex::load(&project.plugin_index_path());
    cli::sync::push_to_plugins(&plugins, &payload, &config, "dev", &mut plugin_index).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 4);
    // First call: op --version (preflight)
    // Second call: op vault get (preflight)
    // Third call: op item get
    assert_eq!(calls[2].program, "op");
    assert!(calls[2].args.contains(&"get".to_string()));
    assert!(calls[2].args.contains(&"testapp - Dev".to_string()));
    // Fourth call: op item create
    assert_eq!(calls[3].program, "op");
    assert!(calls[3].args.contains(&"create".to_string()));
    assert!(calls[3].args.contains(&"testapp - Dev".to_string()));
    assert!(calls[3]
        .args
        .iter()
        .any(|a| a.contains("STRIPE_KEY[concealed]=sk_test_999")));
}

#[test]
fn push_onepassword_edits_existing() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test_999").unwrap();
    let payload = store.payload().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
                                   // op item get → found (return valid JSON)
    let item_json = json!({
        "fields": [
            {"section": {"label": "Stripe"}, "label": "STRIPE_KEY", "value": "sk_old"},
            {"section": {"label": "_Metadata"}, "label": "version", "value": "1"},
        ]
    });
    runner.push_success(serde_json::to_vec(&item_json).unwrap().as_slice(), b"");
    // op item edit → success
    runner.push_success(b"", b"");

    let plugins = esk::plugins::build_plugins(&config, &runner);
    let mut plugin_index = PluginIndex::load(&project.plugin_index_path());
    cli::sync::push_to_plugins(&plugins, &payload, &config, "dev", &mut plugin_index).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 4);
    assert!(calls[3].args.contains(&"edit".to_string()));
    assert!(calls[3]
        .args
        .iter()
        .any(|a| a.contains("STRIPE_KEY[concealed]=sk_test_999")));
}

#[test]
fn push_onepassword_version_metadata() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "val").unwrap();
    let payload = store.payload().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
                                   // op item get → not found
    runner.push_failure(b"isn't an item in vault");
    // op item create → success
    runner.push_success(b"", b"");

    let plugins = esk::plugins::build_plugins(&config, &runner);
    let mut plugin_index = PluginIndex::load(&project.plugin_index_path());
    cli::sync::push_to_plugins(&plugins, &payload, &config, "dev", &mut plugin_index).unwrap();

    let calls = runner.take_calls();
    // The create call should include version metadata
    assert!(calls[3]
        .args
        .iter()
        .any(|a| a == "_Metadata.version[text]=1"));
}

#[test]
fn plugin_sync_onepassword_merges_remote() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "local_val").unwrap();
    // Local is now v1

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
                                   // pull: op item get → returns higher version with different value
    let item_json = json!({
        "fields": [
            {"section": {"label": "Stripe"}, "label": "STRIPE_KEY", "value": "remote_val"},
            {"section": {"label": "_Metadata"}, "label": "version", "value": "5"},
        ]
    });
    runner.push_success(serde_json::to_vec(&item_json).unwrap().as_slice(), b"");
    // push-back of merged result: op item get + op item edit
    let item_json2 = json!({
        "fields": [
            {"section": {"label": "Stripe"}, "label": "STRIPE_KEY", "value": "remote_val"},
            {"section": {"label": "_Metadata"}, "label": "version", "value": "5"},
        ]
    });
    runner.push_success(serde_json::to_vec(&item_json2).unwrap().as_slice(), b"");
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, "dev", None, false, false, false, false, &runner).unwrap();

    // Local store should be updated with remote value
    let store = project.store().unwrap();
    assert_eq!(
        store.get("STRIPE_KEY", "dev").unwrap(),
        Some("remote_val".to_string())
    );
}

#[test]
fn plugin_sync_onepassword_no_item() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "local_val").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
                                   // pull: op item get → not found
    runner.push_failure(b"isn't an item in vault");

    // Should succeed — no remote data, nothing to reconcile
    cli::sync::run_with_runner(&config, "dev", None, false, false, false, false, &runner).unwrap();

    // Local value unchanged
    let store = project.store().unwrap();
    assert_eq!(
        store.get("STRIPE_KEY", "dev").unwrap(),
        Some("local_val".to_string())
    );
}

#[test]
fn plugin_sync_dry_run_no_mutation() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "local_val").unwrap();
    // Local is now v1

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
                                   // pull: op item get → returns higher version
    let item_json = json!({
        "fields": [
            {"section": {"label": "Stripe"}, "label": "STRIPE_KEY", "value": "remote_val"},
            {"section": {"label": "_Metadata"}, "label": "version", "value": "5"},
        ]
    });
    runner.push_success(serde_json::to_vec(&item_json).unwrap().as_slice(), b"");
    // No push-back responses needed — dry-run exits before pushing

    cli::sync::run_with_runner(&config, "dev", None, true, false, false, false, &runner).unwrap();

    // Local store should NOT be updated (dry-run)
    let store = project.store().unwrap();
    assert_eq!(
        store.get("STRIPE_KEY", "dev").unwrap(),
        Some("local_val".to_string())
    );

    // Only 3 calls: preflight (2) + pull (1). No push calls.
    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
}

// === mixed adapter integration ===

#[test]
fn sync_full_config_cloudflare_and_convex() {
    let project = TestProject::with_store(FULL_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    std::fs::create_dir_all(project.root().join("apps/api")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "prod", "sk_live").unwrap();
    store
        .set("CONVEX_URL", "prod", "https://prod.convex.cloud")
        .unwrap();

    let runner = MockCommandRunner::new();
    // preflight: cloudflare (--version + whoami) + convex (--version + env list)
    runner.push_success(b"", b""); // wrangler --version
    runner.push_success(b"", b""); // wrangler whoami
    runner.push_success(b"", b""); // npx --version
    runner.push_success(b"", b""); // convex env list
                                   // env adapter is batch (no command runner calls), but cloudflare + convex each need one
    runner.push_success(b"", b""); // cloudflare: STRIPE_KEY
    runner.push_success(b"", b""); // convex: CONVEX_URL

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 6);
    // Skip preflight calls (4), check that both wrangler and npx were called for sync
    let sync_programs: Vec<&str> = calls[4..].iter().map(|c| c.program.as_str()).collect();
    assert!(sync_programs.contains(&"wrangler"));
    assert!(sync_programs.contains(&"npx"));
}

#[test]
fn sync_cloudflare_force_resyncs() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b"");

    // First sync
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 3); // preflight (2) + sync

    // Force sync — should re-run despite unchanged hash
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), true, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 3); // preflight (2) + sync
}

// === plugin tracker integration ===

#[test]
fn push_records_plugin_index() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    let payload = store.payload().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
    runner.push_failure(b"isn't an item in vault"); // op item get
    runner.push_success(b"", b""); // op item create

    let plugins = esk::plugins::build_plugins(&config, &runner);
    let mut plugin_index = PluginIndex::load(&project.plugin_index_path());
    cli::sync::push_to_plugins(&plugins, &payload, &config, "dev", &mut plugin_index).unwrap();
    plugin_index.save().unwrap();

    let index = PluginIndex::load(&project.plugin_index_path());
    assert_eq!(index.records.len(), 1);
    let record = &index.records["onepassword:dev"];
    assert_eq!(record.plugin, "onepassword");
    assert_eq!(record.environment, "dev");
    assert_eq!(record.pushed_version, 1);
    assert_eq!(
        record.last_push_status,
        esk::plugin_tracker::PushStatus::Success
    );
}

#[test]
fn push_records_failure_in_plugin_index() {
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
    let store = project.store().unwrap();
    store.set("KEY", "dev", "val").unwrap();
    let payload = store.payload().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
    runner.push_failure(b"isn't an item in vault"); // op item get
    runner.push_failure(b"op create failed"); // op item create fails

    let plugins = esk::plugins::build_plugins(&config, &runner);
    let mut plugin_index = PluginIndex::load(&project.plugin_index_path());
    let failures =
        cli::sync::push_to_plugins(&plugins, &payload, &config, "dev", &mut plugin_index).unwrap();
    plugin_index.save().unwrap();
    assert!(failures > 0);

    let index = PluginIndex::load(&project.plugin_index_path());
    assert_eq!(index.records.len(), 1);
    let record = &index.records["onepassword:dev"];
    assert_eq!(
        record.last_push_status,
        esk::plugin_tracker::PushStatus::Failed
    );
    assert!(record.last_error.is_some());
}

#[test]
fn status_shows_plugin_section() {
    let project = TestProject::with_store(PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();

    // No push yet — should show "never pushed"
    cli::status::run(&config, None, false).unwrap();
}

#[test]
fn status_shows_pushed_plugin() {
    let project = TestProject::with_store(PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    let payload = store.payload().unwrap();

    // Manually write a plugin index with a pushed record
    let mut index = PluginIndex::new(&project.plugin_index_path());
    index.record_success("onepassword", "dev", payload.version);
    index.save().unwrap();

    cli::status::run(&config, Some("dev"), false).unwrap();
}

#[test]
fn status_shows_stale_plugin() {
    let project = TestProject::with_store(PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    // Push at v0, then bump store version
    let mut index = PluginIndex::new(&project.plugin_index_path());
    index.record_success("onepassword", "dev", 0);
    index.save().unwrap();

    store.set("KEY", "dev", "val").unwrap(); // bumps to v1

    cli::status::run(&config, Some("dev"), false).unwrap();
}

#[test]
fn status_plugin_env_filter() {
    let project = TestProject::with_store(PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();

    let mut index = PluginIndex::new(&project.plugin_index_path());
    index.record_success("onepassword", "dev", 1);
    index.record_success("onepassword", "prod", 1);
    index.save().unwrap();

    // Filter to dev only — should not error
    cli::status::run(&config, Some("dev"), false).unwrap();
}

#[test]
fn status_dashboard_healthy() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();

    store.set("MY_SECRET", "dev", "val").unwrap();
    store.set("OTHER_SECRET", "dev", "val2").unwrap();
    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();

    // All synced — dashboard should render without error
    cli::status::run(&config, Some("dev"), false).unwrap();
    cli::status::run(&config, Some("dev"), true).unwrap();
}

#[test]
fn status_dashboard_coverage_gap() {
    // ENV_ONLY_CONFIG has MY_SECRET targeting web:dev and web:prod
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    // Set in dev but not prod → coverage gap
    store.set("MY_SECRET", "dev", "val").unwrap();

    cli::status::run(&config, None, false).unwrap();
}

#[test]
fn status_dashboard_orphan() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    // Set a key that's not in config → orphan
    store.set("ROGUE_KEY", "dev", "val").unwrap();

    cli::status::run(&config, None, false).unwrap();
}

#[test]
fn status_dashboard_adapter_health() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_error("wrangler not found"); // preflight fails

    // Should not panic even with failing adapter
    cli::status::run_with_runner(&config, None, false, &runner).unwrap();
}

#[test]
fn status_dashboard_next_steps() {
    let project = TestProject::with_store(ENV_ONLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();

    // Set and sync, then change to create pending state
    store.set("MY_SECRET", "dev", "val").unwrap();
    cli::deploy::run(&config, Some("dev"), false, false, false).unwrap();
    store.set("MY_SECRET", "dev", "changed").unwrap();

    // Should render with next steps (pending sync)
    cli::status::run(&config, None, false).unwrap();
}

#[test]
fn set_auto_push_records_plugin_index() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    runner.push_success(b"", b""); // preflight: op vault get
    runner.push_failure(b"isn't an item in vault"); // op item get
    runner.push_success(b"", b""); // op item create

    // no_sync=false so auto-push runs (no adapters configured, sync is a no-op)
    cli::set::run_with_runner(
        &config,
        "STRIPE_KEY",
        "dev",
        Some("val"),
        None,
        false,
        false,
        &runner,
    )
    .unwrap();

    let index = PluginIndex::load(&project.plugin_index_path());
    assert_eq!(index.records.len(), 1);
    let record = &index.records["onepassword:dev"];
    assert_eq!(
        record.last_push_status,
        esk::plugin_tracker::PushStatus::Success
    );
}

// === tombstone delete tracking ===

#[test]
fn sync_records_tombstone_delete_success() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    // Sync to establish initial state
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b""); // sync_secret STRIPE_KEY
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    // Delete the key (creates tombstone)
    store.delete("STRIPE_KEY", "dev").unwrap();

    // Sync again — should call delete_secret and record tombstone
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b""); // delete_secret STRIPE_KEY
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    let tracker_key = SyncIndex::tracker_key("STRIPE_KEY", "cloudflare", Some("web"), "dev");
    let record = index.records.get(&tracker_key).unwrap();
    assert_eq!(record.value_hash, SyncIndex::TOMBSTONE_HASH);
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Success);
}

#[test]
fn sync_records_tombstone_delete_failure() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    store.delete("STRIPE_KEY", "dev").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_failure(b"delete failed"); // delete_secret fails
    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let tracker_key = SyncIndex::tracker_key("STRIPE_KEY", "cloudflare", Some("web"), "dev");
    let record = index.records.get(&tracker_key).unwrap();
    assert_eq!(record.value_hash, SyncIndex::TOMBSTONE_HASH);
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_retries_failed_tombstone_delete() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    store.delete("STRIPE_KEY", "dev").unwrap();

    // First sync: delete fails
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_failure(b"delete failed");
    let _ = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner);

    // Second sync: delete succeeds — should retry because previous was failed
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b""); // delete_secret succeeds
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    let tracker_key = SyncIndex::tracker_key("STRIPE_KEY", "cloudflare", Some("web"), "dev");
    let record = index.records.get(&tracker_key).unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Success);
}

#[test]
fn sync_skips_already_deleted_tombstone() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    store.delete("STRIPE_KEY", "dev").unwrap();

    // First sync: delete succeeds
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b""); // delete_secret
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    // Second sync: should skip (already successfully deleted)
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    // Verify no additional calls were made beyond preflight
    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2); // wrangler --version + wrangler whoami
    assert_eq!(calls[0].args[0], "--version");
    assert_eq!(calls[1].args[0], "whoami");
}

#[test]
fn delete_then_recreate_same_value_syncs() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();

    // Set and sync
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b""); // sync_secret
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    // Delete and sync (tombstone processed)
    store.delete("STRIPE_KEY", "dev").unwrap();
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b""); // delete_secret
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    // Recreate with same value
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: wrangler --version
    runner.push_success(b"", b""); // preflight: wrangler whoami
    runner.push_success(b"", b""); // sync_secret — must NOT be skipped
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    // Verify sync_secret was called (3 calls: preflight x2 + sync)
    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    // Third call should be wrangler secret put
    assert!(calls[2].args.contains(&"put".to_string()));
}

// === fly adapter integration tests ===

#[test]
fn sync_fly_calls_cli() {
    let project = TestProject::with_store(FLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // fly --version
    runner.push_success(b"", b""); // fly auth whoami
    runner.push_success(b"", b""); // fly secrets set

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "fly");
    assert_eq!(calls[2].args, vec!["secrets", "import", "-a", "my-fly-app"]);
    // Secret value passed via stdin, not in args
    let stdin = calls[2].stdin.as_ref().expect("stdin should be set");
    assert_eq!(stdin, b"API_KEY=secret123\n");
}

#[test]
fn sync_fly_prod_env_flags() {
    let project = TestProject::with_store(FLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // fly --version
    runner.push_success(b"", b""); // fly auth whoami
    runner.push_success(b"", b""); // fly secrets import

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec!["secrets", "import", "-a", "my-fly-app", "--stage"]
    );
    let stdin = calls[2].stdin.as_ref().expect("stdin should be set");
    assert_eq!(stdin, b"API_KEY=secret456\n");
}

#[test]
fn sync_fly_records_tracker() {
    let project = TestProject::with_store(FLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // fly --version
    runner.push_success(b"", b""); // fly auth whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("fly")));
}

#[test]
fn sync_fly_failure_tracked() {
    let project = TestProject::with_store(FLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // fly --version
    runner.push_success(b"", b""); // fly auth whoami
    runner.push_failure(b"deploy error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("fly"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_fly_skip_unchanged() {
    let project = TestProject::with_store(FLY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    // First sync
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // fly --version
    runner.push_success(b"", b""); // fly auth whoami
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    // Second sync (unchanged)
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // fly --version
    runner.push_success(b"", b""); // fly auth whoami
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2); // only preflight, no sync
}

// === netlify adapter integration tests ===

#[test]
fn sync_netlify_calls_cli() {
    let project = TestProject::with_store(NETLIFY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // netlify --version
    runner.push_success(b"", b""); // netlify status
    runner.push_success(b"", b""); // netlify env:set

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "netlify");
    assert_eq!(
        calls[2].args,
        vec!["env:set", "API_KEY", "secret123", "--site", "my-site-id"]
    );
}

#[test]
fn sync_netlify_prod_env_flags() {
    let project = TestProject::with_store(NETLIFY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // netlify --version
    runner.push_success(b"", b""); // netlify status
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec![
            "env:set",
            "API_KEY",
            "secret456",
            "--site",
            "my-site-id",
            "--context",
            "production"
        ]
    );
}

#[test]
fn sync_netlify_records_tracker() {
    let project = TestProject::with_store(NETLIFY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // netlify --version
    runner.push_success(b"", b""); // netlify status
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("netlify")));
}

#[test]
fn sync_netlify_failure_tracked() {
    let project = TestProject::with_store(NETLIFY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // netlify --version
    runner.push_success(b"", b""); // netlify status
    runner.push_failure(b"auth error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("netlify"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_netlify_skip_unchanged() {
    let project = TestProject::with_store(NETLIFY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // netlify --version
    runner.push_success(b"", b""); // netlify status
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // netlify --version
    runner.push_success(b"", b""); // netlify status
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
}

// === vercel adapter integration tests ===

#[test]
fn sync_vercel_calls_cli() {
    let project = TestProject::with_store(VERCEL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // vercel --version
    runner.push_success(b"", b""); // vercel whoami
    runner.push_success(b"", b""); // vercel env add

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "vercel");
    assert_eq!(
        calls[2].args,
        vec!["env", "add", "API_KEY", "development", "--force"]
    );
    assert_eq!(calls[2].stdin.as_ref().unwrap(), b"secret123");
}

#[test]
fn sync_vercel_prod_env_flags() {
    let project = TestProject::with_store(VERCEL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // vercel --version
    runner.push_success(b"", b""); // vercel whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec![
            "env",
            "add",
            "API_KEY",
            "production",
            "--force",
            "--scope",
            "my-team"
        ]
    );
}

#[test]
fn sync_vercel_records_tracker() {
    let project = TestProject::with_store(VERCEL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // vercel --version
    runner.push_success(b"", b""); // vercel whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("vercel")));
}

#[test]
fn sync_vercel_failure_tracked() {
    let project = TestProject::with_store(VERCEL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // vercel --version
    runner.push_success(b"", b""); // vercel whoami
    runner.push_failure(b"auth error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("vercel"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_vercel_skip_unchanged() {
    let project = TestProject::with_store(VERCEL_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // vercel --version
    runner.push_success(b"", b""); // vercel whoami
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // vercel --version
    runner.push_success(b"", b""); // vercel whoami
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
}

// === github adapter integration tests ===

#[test]
fn sync_github_calls_cli() {
    let project = TestProject::with_store(GITHUB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // gh --version
    runner.push_success(b"", b""); // gh auth status
    runner.push_success(b"", b""); // gh secret set

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "gh");
    assert_eq!(
        calls[2].args,
        vec!["secret", "set", "API_KEY", "-R", "owner/repo"]
    );
    assert_eq!(calls[2].stdin.as_ref().unwrap(), b"secret123");
}

#[test]
fn sync_github_prod_env_flags() {
    let project = TestProject::with_store(GITHUB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // gh --version
    runner.push_success(b"", b""); // gh auth status
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec![
            "secret",
            "set",
            "API_KEY",
            "-R",
            "owner/repo",
            "--env",
            "production"
        ]
    );
}

#[test]
fn sync_github_records_tracker() {
    let project = TestProject::with_store(GITHUB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // gh --version
    runner.push_success(b"", b""); // gh auth status
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("github")));
}

#[test]
fn sync_github_failure_tracked() {
    let project = TestProject::with_store(GITHUB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // gh --version
    runner.push_success(b"", b""); // gh auth status
    runner.push_failure(b"auth error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("github"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_github_skip_unchanged() {
    let project = TestProject::with_store(GITHUB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // gh --version
    runner.push_success(b"", b""); // gh auth status
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // gh --version
    runner.push_success(b"", b""); // gh auth status
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
}

// === heroku adapter integration tests ===

#[test]
fn sync_heroku_calls_cli() {
    let project = TestProject::with_store(HEROKU_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // heroku --version
    runner.push_success(b"", b""); // heroku auth:whoami
    runner.push_success(b"", b""); // heroku config:set

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "heroku");
    assert_eq!(
        calls[2].args,
        vec!["config:set", "API_KEY=secret123", "-a", "my-heroku-app"]
    );
}

#[test]
fn sync_heroku_prod_env_flags() {
    let project = TestProject::with_store(HEROKU_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // heroku --version
    runner.push_success(b"", b""); // heroku auth:whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec![
            "config:set",
            "API_KEY=secret456",
            "-a",
            "my-heroku-app",
            "--remote",
            "staging"
        ]
    );
}

#[test]
fn sync_heroku_records_tracker() {
    let project = TestProject::with_store(HEROKU_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // heroku --version
    runner.push_success(b"", b""); // heroku auth:whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("heroku")));
}

#[test]
fn sync_heroku_failure_tracked() {
    let project = TestProject::with_store(HEROKU_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // heroku --version
    runner.push_success(b"", b""); // heroku auth:whoami
    runner.push_failure(b"auth error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("heroku"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_heroku_skip_unchanged() {
    let project = TestProject::with_store(HEROKU_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // heroku --version
    runner.push_success(b"", b""); // heroku auth:whoami
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // heroku --version
    runner.push_success(b"", b""); // heroku auth:whoami
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
}

// === supabase adapter integration tests ===

#[test]
fn sync_supabase_calls_cli() {
    let project = TestProject::with_store(SUPABASE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // supabase --version
    runner.push_success(b"", b""); // supabase secrets list (preflight)
    runner.push_success(b"", b""); // supabase secrets set

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "supabase");
    assert_eq!(
        calls[2].args,
        vec!["secrets", "set", "--project-ref", "abcdef123456"]
    );
    // Secret value passed via stdin, not in args
    let stdin = calls[2].stdin.as_ref().expect("stdin should be set");
    assert_eq!(stdin, b"API_KEY=secret123\n");
}

#[test]
fn sync_supabase_prod_env_flags() {
    let project = TestProject::with_store(SUPABASE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // supabase --version
    runner.push_success(b"", b""); // supabase secrets list (preflight)
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec![
            "secrets",
            "set",
            "--project-ref",
            "abcdef123456",
            "--experimental"
        ]
    );
    let stdin = calls[2].stdin.as_ref().expect("stdin should be set");
    assert_eq!(stdin, b"API_KEY=secret456\n");
}

#[test]
fn sync_supabase_records_tracker() {
    let project = TestProject::with_store(SUPABASE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // supabase --version
    runner.push_success(b"", b""); // supabase secrets list (preflight)
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("supabase")));
}

#[test]
fn sync_supabase_failure_tracked() {
    let project = TestProject::with_store(SUPABASE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // supabase --version
    runner.push_success(b"", b""); // supabase secrets list (preflight)
    runner.push_failure(b"api error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("supabase"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_supabase_skip_unchanged() {
    let project = TestProject::with_store(SUPABASE_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // supabase --version
    runner.push_success(b"", b""); // supabase secrets list (preflight)
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // supabase --version
    runner.push_success(b"", b""); // supabase secrets list (preflight)
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2); // only preflight (version check + secrets list)
}

// === railway adapter integration tests ===

#[test]
fn sync_railway_calls_cli() {
    let project = TestProject::with_store(RAILWAY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // railway --version
    runner.push_success(b"", b""); // railway whoami
    runner.push_success(b"", b""); // railway variables --set

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "railway");
    assert_eq!(
        calls[2].args,
        vec!["variables", "--set", "API_KEY=secret123"]
    );
}

#[test]
fn sync_railway_prod_env_flags() {
    let project = TestProject::with_store(RAILWAY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // railway --version
    runner.push_success(b"", b""); // railway whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec![
            "variables",
            "--set",
            "API_KEY=secret456",
            "--environment",
            "production"
        ]
    );
}

#[test]
fn sync_railway_records_tracker() {
    let project = TestProject::with_store(RAILWAY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // railway --version
    runner.push_success(b"", b""); // railway whoami
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("railway")));
}

#[test]
fn sync_railway_failure_tracked() {
    let project = TestProject::with_store(RAILWAY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // railway --version
    runner.push_success(b"", b""); // railway whoami
    runner.push_failure(b"api error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("railway"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_railway_skip_unchanged() {
    let project = TestProject::with_store(RAILWAY_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // railway --version
    runner.push_success(b"", b""); // railway whoami
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // railway --version
    runner.push_success(b"", b""); // railway whoami
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
}

// === gitlab adapter integration tests ===

#[test]
fn sync_gitlab_calls_cli() {
    let project = TestProject::with_store(GITLAB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret123").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // glab --version
    runner.push_success(b"", b""); // glab auth status
    runner.push_success(b"", b""); // glab variable set

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].program, "glab");
    assert_eq!(
        calls[2].args,
        vec!["variable", "set", "API_KEY", "--scope", "dev"]
    );
    // Secret value passed via stdin, not in args
    let stdin = calls[2].stdin.as_ref().expect("stdin should be set");
    assert_eq!(stdin, b"secret123");
}

#[test]
fn sync_gitlab_prod_env_flags() {
    let project = TestProject::with_store(GITLAB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "prod", "secret456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // glab --version
    runner.push_success(b"", b""); // glab auth status
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(
        calls[2].args,
        vec!["variable", "set", "API_KEY", "--scope", "prod", "--masked"]
    );
    let stdin = calls[2].stdin.as_ref().expect("stdin should be set");
    assert_eq!(stdin, b"secret456");
}

#[test]
fn sync_gitlab_records_tracker() {
    let project = TestProject::with_store(GITLAB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // glab --version
    runner.push_success(b"", b""); // glab auth status
    runner.push_success(b"", b"");

    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path());
    assert!(index
        .records
        .keys()
        .any(|k| k.contains("API_KEY") && k.contains("gitlab")));
}

#[test]
fn sync_gitlab_failure_tracked() {
    let project = TestProject::with_store(GITLAB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // glab --version
    runner.push_success(b"", b""); // glab auth status
    runner.push_failure(b"api error");

    let err = cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner)
        .unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path());
    let record = index
        .records
        .values()
        .find(|r| r.target.contains("gitlab"))
        .unwrap();
    assert_eq!(record.last_sync_status, esk::tracker::SyncStatus::Failed);
}

#[test]
fn sync_gitlab_skip_unchanged() {
    let project = TestProject::with_store(GITLAB_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("API_KEY", "dev", "secret").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // glab --version
    runner.push_success(b"", b""); // glab auth status
    runner.push_success(b"", b"");
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // glab --version
    runner.push_success(b"", b""); // glab auth status
    cli::deploy::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
}
