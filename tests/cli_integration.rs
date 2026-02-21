mod helpers;

use helpers::*;
use lockbox::cli;
use lockbox::tracker::SyncIndex;
use serde_json::json;

// === init ===

#[test]
fn init_creates_all_files() {
    let dir = tempfile::tempdir().unwrap();
    cli::init::run(dir.path()).unwrap();
    assert!(dir.path().join("lockbox.yaml").is_file());
    assert!(dir.path().join(".lockbox/store.enc").is_file());
    assert!(dir.path().join(".lockbox/store.key").is_file());
    assert!(dir.path().join(".lockbox/sync-index.json").is_file());
}

#[test]
fn init_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    cli::init::run(dir.path()).unwrap();
    let key1 = std::fs::read_to_string(dir.path().join(".lockbox/store.key")).unwrap();
    cli::init::run(dir.path()).unwrap();
    let key2 = std::fs::read_to_string(dir.path().join(".lockbox/store.key")).unwrap();
    assert_eq!(key1, key2); // Key not overwritten
}

#[test]
fn init_gitignore_warning() {
    let dir = tempfile::tempdir().unwrap();
    // Create .gitignore without .lockbox/store.key
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
    let err = cli::push::run_with_runner(&config, "dev", Some("nonexistent"), &runner).unwrap_err();
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
    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    let err = cli::pull::run_with_runner(&config, "dev", Some("nonexistent"), false, &runner).unwrap_err();
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
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].program, "wrangler");
    assert_eq!(calls[1].args, vec!["secret", "put", "STRIPE_KEY"]);
    assert_eq!(calls[1].cwd.as_ref().unwrap(), &project.root().join("apps/web"));
    assert_eq!(calls[1].stdin.as_ref().unwrap(), b"sk_test_123");
}

#[test]
fn sync_cloudflare_prod_env_flags() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "prod", "sk_live_456").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].args,
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
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let index = SyncIndex::load(&project.sync_index_path()).unwrap();
    assert!(!index.records.is_empty());
    assert!(index.records.keys().any(|k| k.contains("STRIPE_KEY") && k.contains("cloudflare")));
}

#[test]
fn sync_cloudflare_failure_tracked() {
    let project = TestProject::with_store(CLOUDFLARE_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/web")).unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "sk_test").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    runner.push_failure(b"auth error");

    let err =
        cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path()).unwrap();
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
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    // Skip preflight call (index 0), check sync calls
    let keys: Vec<&str> = calls[1..].iter().map(|c| c.args[2].as_str()).collect();
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
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");

    // First sync
    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 2); // preflight + sync

    // Second sync — same value, should skip (only preflight call)
    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 1); // preflight only
}

// === convex adapter integration ===

#[test]
fn sync_convex_calls_npx() {
    let project = TestProject::with_store(CONVEX_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/api")).unwrap();
    let store = project.store().unwrap();
    store.set("CONVEX_URL", "dev", "https://dev.convex.cloud").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].program, "npx");
    assert_eq!(
        calls[1].args,
        vec!["convex", "env", "set", "CONVEX_URL", "https://dev.convex.cloud"]
    );
    assert_eq!(calls[1].cwd.as_ref().unwrap(), &project.root().join("apps/api"));
}

#[test]
fn sync_convex_prod_env_flags() {
    let project = TestProject::with_store(CONVEX_CONFIG).unwrap();
    let config = project.config().unwrap();
    std::fs::create_dir_all(project.root().join("apps/api")).unwrap();
    let store = project.store().unwrap();
    store.set("CONVEX_URL", "prod", "https://prod.convex.cloud").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[1].args,
        vec!["convex", "env", "set", "CONVEX_URL", "https://prod.convex.cloud", "--prod"]
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
    store.set("CONVEX_URL", "dev", "https://dev.convex.cloud").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");

    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 2);
    assert!(calls[1].env.contains(&(
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
    store.set("CONVEX_URL", "dev", "https://dev.convex.cloud").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    runner.push_failure(b"deploy error");

    let err =
        cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap_err();
    assert!(err.to_string().contains("failed"));

    let index = SyncIndex::load(&project.sync_index_path()).unwrap();
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

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight: op --version
    // op item get → not found
    runner.push_failure(b"isn't an item in vault");
    // op item create → success
    runner.push_success(b"", b"");

    cli::push::run_with_runner(&config, "dev", None, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    // First call: op --version (preflight)
    // Second call: op item get
    assert_eq!(calls[1].program, "op");
    assert!(calls[1].args.contains(&"get".to_string()));
    assert!(calls[1].args.contains(&"testapp - Dev".to_string()));
    // Third call: op item create
    assert_eq!(calls[2].program, "op");
    assert!(calls[2].args.contains(&"create".to_string()));
    assert!(calls[2].args.contains(&"testapp - Dev".to_string()));
    assert!(calls[2]
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

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
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

    cli::push::run_with_runner(&config, "dev", None, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 3);
    assert!(calls[2].args.contains(&"edit".to_string()));
    assert!(calls[2]
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

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    // op item get → not found
    runner.push_failure(b"isn't an item in vault");
    // op item create → success
    runner.push_success(b"", b"");

    cli::push::run_with_runner(&config, "dev", None, &runner).unwrap();

    let calls = runner.take_calls();
    // The create call should include version metadata
    assert!(calls[2]
        .args
        .iter()
        .any(|a| a == "_Metadata.version[text]=1"));
}

#[test]
fn pull_onepassword_merges_remote() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "local_val").unwrap();
    // Local is now v1

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    // op item get → returns higher version with different value
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

    cli::pull::run_with_runner(&config, "dev", None, false, &runner).unwrap();

    // Local store should be updated with remote value
    let store = project.store().unwrap();
    assert_eq!(
        store.get("STRIPE_KEY", "dev").unwrap(),
        Some("remote_val".to_string())
    );
}

#[test]
fn pull_onepassword_no_item() {
    let project = TestProject::with_store(ONEPASSWORD_PLUGIN_CONFIG).unwrap();
    let config = project.config().unwrap();
    let store = project.store().unwrap();
    store.set("STRIPE_KEY", "dev", "local_val").unwrap();

    let runner = MockCommandRunner::new();
    runner.push_success(b"", b""); // preflight
    // op item get → not found
    runner.push_failure(b"isn't an item in vault");

    // Should succeed — no remote data, nothing to reconcile
    cli::pull::run_with_runner(&config, "dev", None, false, &runner).unwrap();

    // Local value unchanged
    let store = project.store().unwrap();
    assert_eq!(
        store.get("STRIPE_KEY", "dev").unwrap(),
        Some("local_val".to_string())
    );
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
    store.set("CONVEX_URL", "prod", "https://prod.convex.cloud").unwrap();

    let runner = MockCommandRunner::new();
    // preflight: cloudflare + convex
    runner.push_success(b"", b""); // wrangler --version
    runner.push_success(b"", b""); // npx --version
    // env adapter is batch (no command runner calls), but cloudflare + convex each need one
    runner.push_success(b"", b""); // cloudflare: STRIPE_KEY
    runner.push_success(b"", b""); // convex: CONVEX_URL

    cli::sync::run_with_runner(&config, Some("prod"), false, false, false, &runner).unwrap();

    let calls = runner.take_calls();
    assert_eq!(calls.len(), 4);
    // Skip preflight calls, check that both wrangler and npx were called for sync
    let sync_programs: Vec<&str> = calls[2..].iter().map(|c| c.program.as_str()).collect();
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
    runner.push_success(b"", b""); // preflight
    runner.push_success(b"", b"");

    // First sync
    cli::sync::run_with_runner(&config, Some("dev"), false, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 2); // preflight + sync

    // Force sync — should re-run despite unchanged hash
    runner.push_success(b"", b""); // preflight (new build_sync_adapters call)
    runner.push_success(b"", b"");
    cli::sync::run_with_runner(&config, Some("dev"), true, false, false, &runner).unwrap();
    assert_eq!(runner.take_calls().len(), 2); // preflight + sync
}
