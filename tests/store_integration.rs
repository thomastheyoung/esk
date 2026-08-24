mod helpers;

use esk::store::SecretStore;
use helpers::*;

#[test]
fn store_full_lifecycle() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    store.set("A", "dev", "val_a").unwrap();
    store.set("B", "dev", "val_b").unwrap();
    store.set("C", "dev", "val_c").unwrap();

    let list = store.list().unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(store.get("A", "dev").unwrap(), Some("val_a".to_string()));
    assert_eq!(store.get("B", "dev").unwrap(), Some("val_b".to_string()));
    assert_eq!(store.get("C", "dev").unwrap(), Some("val_c".to_string()));
}

#[test]
fn store_reopen_after_set() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    {
        let store = project.store().unwrap();
        store.set("KEY", "dev", "secret_value").unwrap();
    }
    // Open a new handle
    let store2 = SecretStore::open(project.root()).unwrap();
    assert_eq!(
        store2.get("KEY", "dev").unwrap(),
        Some("secret_value".to_string())
    );
}

#[test]
fn cli_reopens_store_with_esk_store_key_without_key_file() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    store.set("KEY", "dev", "value-from-ci").unwrap();
    let key = std::fs::read_to_string(project.root().join(".esk/store.key")).unwrap();
    std::fs::remove_file(project.root().join(".esk/store.key")).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_esk"))
        .current_dir(project.root())
        .env("ESK_STORE_KEY", key.trim())
        .args(["get", "KEY", "--env", "dev"])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "value-from-ci"
    );

    let rotate = std::process::Command::new(env!("CARGO_BIN_EXE_esk"))
        .current_dir(project.root())
        .env("ESK_STORE_KEY", key.trim())
        .args(["key", "rotate"])
        .output()
        .unwrap();
    assert!(!rotate.status.success());
    assert!(String::from_utf8_lossy(&rotate.stderr).contains("cannot rotate"));
}

#[test]
fn cli_rejects_malformed_esk_store_key_instead_of_falling_back() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_esk"))
        .current_dir(project.root())
        .env("ESK_STORE_KEY", "not-a-key")
        .args(["get", "KEY", "--env", "dev"])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid ESK_STORE_KEY"));
}

#[test]
fn init_keeps_explicit_file_provider_when_esk_store_key_is_set() {
    let dir = tempfile::tempdir().unwrap();
    let environment_key = hex::encode([0x5a_u8; 32]);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_esk"))
        .current_dir(dir.path())
        .env("ESK_STORE_KEY", &environment_key)
        .arg("init")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let file_key = std::fs::read_to_string(dir.path().join(".esk/store.key")).unwrap();
    assert_ne!(file_key.trim(), environment_key);
    assert!(dir.path().join(".esk/store.enc").is_file());
}

#[test]
fn store_large_payload() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    for i in 0..100 {
        store
            .set(&format!("KEY_{i}"), "dev", &format!("value_{i}"))
            .unwrap();
    }
    let list = store.list().unwrap();
    assert_eq!(list.len(), 100);

    // Reopen and verify
    let store2 = SecretStore::open(project.root()).unwrap();
    for i in 0..100 {
        assert_eq!(
            store2.get(&format!("KEY_{i}"), "dev").unwrap(),
            Some(format!("value_{i}"))
        );
    }
}

#[test]
fn store_empty_value() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    store.set("EMPTY", "dev", "").unwrap();
    assert_eq!(store.get("EMPTY", "dev").unwrap(), Some(String::new()));
}

#[test]
fn store_rejects_invalid_key_characters() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    let err = store.set("MY.KEY-WITH_SPECIAL", "dev", "val").unwrap_err();
    assert!(err.to_string().contains("invalid secret key"));
    // Underscores and alphanumeric are fine
    store.set("MY_KEY_WITH_UNDERSCORE", "dev", "val").unwrap();
}

#[test]
fn store_version_monotonic() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    for i in 1..=10 {
        let payload = store.set(&format!("K{i}"), "dev", "v").unwrap();
        assert_eq!(payload.version, i as u64);
    }
}

#[test]
fn stale_payload_commit_is_rejected_without_overwriting_new_value() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    let stale = store.payload().unwrap();
    store.set("NEW", "dev", "fresh").unwrap();

    let mut replacement = stale;
    replacement
        .secrets
        .insert("STALE:dev".into(), "lost".into());
    assert!(!store
        .set_payload_if_version(replacement.version, &replacement)
        .unwrap());
    assert_eq!(store.get("NEW", "dev").unwrap().as_deref(), Some("fresh"));
    assert_eq!(store.get("STALE", "dev").unwrap(), None);
}

#[test]
fn store_concurrent_reads() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    store.set("KEY", "dev", "val").unwrap();

    let store1 = SecretStore::open(project.root()).unwrap();
    let store2 = SecretStore::open(project.root()).unwrap();
    assert_eq!(store1.get("KEY", "dev").unwrap(), Some("val".to_string()));
    assert_eq!(store2.get("KEY", "dev").unwrap(), Some("val".to_string()));
}

#[test]
fn store_concurrent_writers_preserve_all_values_and_versions() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let root = project.root().to_path_buf();
    let workers = 4;
    let writes_per_worker = 8;

    std::thread::scope(|scope| {
        for worker in 0..workers {
            let root = &root;
            scope.spawn(move || {
                let store = SecretStore::open(root).unwrap();
                for write in 0..writes_per_worker {
                    store
                        .set(
                            &format!("WORKER_{worker}_{write}"),
                            "dev",
                            &format!("value_{worker}_{write}"),
                        )
                        .unwrap();
                }
            });
        }
    });

    let store = SecretStore::open(&root).unwrap();
    let payload = store.payload().unwrap();
    assert_eq!(payload.version, (workers * writes_per_worker) as u64);
    assert_eq!(payload.secrets.len(), workers * writes_per_worker);
}

#[test]
fn store_overwrite_preserves_others() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    store.set("A", "dev", "a_val").unwrap();
    store.set("B", "dev", "b_val").unwrap();
    store.set("A", "dev", "new_a").unwrap();
    assert_eq!(store.get("A", "dev").unwrap(), Some("new_a".to_string()));
    assert_eq!(store.get("B", "dev").unwrap(), Some("b_val".to_string()));
}
