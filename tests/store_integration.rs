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
fn store_overwrite_preserves_others() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();
    store.set("A", "dev", "a_val").unwrap();
    store.set("B", "dev", "b_val").unwrap();
    store.set("A", "dev", "new_a").unwrap();
    assert_eq!(store.get("A", "dev").unwrap(), Some("new_a".to_string()));
    assert_eq!(store.get("B", "dev").unwrap(), Some("b_val".to_string()));
}
