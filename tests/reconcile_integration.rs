mod helpers;

use esk::reconcile::{reconcile, ReconcileAction};
use helpers::*;
use std::collections::BTreeMap;

#[test]
fn reconcile_pull_then_push_flow() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();

    // Local is at v0 (empty)
    let payload = store.payload().unwrap();
    assert_eq!(payload.version, 0);

    // Remote has secrets at v3
    let mut remote = BTreeMap::new();
    remote.insert("API_KEY".to_string(), "remote_val".to_string());
    let result = reconcile(&payload, &remote, 3, "dev").unwrap();

    assert_eq!(result.action, ReconcileAction::PullRemote);
    assert_eq!(result.pulled, vec!["API_KEY"]);

    // Write merged payload
    let merged = result.merged_payload.unwrap();
    assert_eq!(merged.version, 3);
    store.set_payload(&merged).unwrap();

    // Now local is at v3 — set a new secret to make it v4
    let updated = store.set("NEW_KEY", "dev", "new_val").unwrap();
    assert_eq!(updated.version, 4);

    // Reconcile again: local is newer
    let result2 = reconcile(&updated, &remote, 3, "dev").unwrap();
    assert_eq!(result2.action, ReconcileAction::PushLocal);
    assert!(result2.pushed.contains(&"NEW_KEY".to_string()));
}

#[test]
fn reconcile_bidirectional_merge() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();

    // Local has A and C
    store.set("A", "dev", "a_local").unwrap();
    store.set("C", "dev", "c_local").unwrap();
    let payload = store.payload().unwrap(); // v2

    // Remote has A and B at v5
    let mut remote = BTreeMap::new();
    remote.insert("A".to_string(), "a_remote".to_string());
    remote.insert("B".to_string(), "b_remote".to_string());

    let result = reconcile(&payload, &remote, 5, "dev").unwrap();
    assert_eq!(result.action, ReconcileAction::PullRemote);

    // A and B pulled from remote
    assert!(result.pulled.contains(&"A".to_string()));
    assert!(result.pulled.contains(&"B".to_string()));

    // C is local-only, should be pushed
    assert_eq!(result.pushed, vec!["C"]);

    let merged = result.merged_payload.unwrap();
    assert_eq!(merged.secrets.get("A:dev").unwrap(), "a_remote");
    assert_eq!(merged.secrets.get("B:dev").unwrap(), "b_remote");
    assert_eq!(merged.secrets.get("C:dev").unwrap(), "c_local");
    // Local-only means version = remote + 1
    assert_eq!(merged.version, 6);
}

#[test]
fn reconcile_preserves_non_target_env() {
    let project = TestProject::with_store(MINIMAL_CONFIG).unwrap();
    let store = project.store().unwrap();

    // Local has secrets for both dev and prod
    store.set("KEY", "dev", "dev_val").unwrap();
    store.set("KEY", "prod", "prod_val").unwrap();
    let payload = store.payload().unwrap(); // v2

    // Remote has different dev value at v5
    let mut remote = BTreeMap::new();
    remote.insert("KEY".to_string(), "new_dev".to_string());

    let result = reconcile(&payload, &remote, 5, "dev").unwrap();
    let merged = result.merged_payload.unwrap();
    assert_eq!(merged.secrets.get("KEY:dev").unwrap(), "new_dev");
    assert_eq!(merged.secrets.get("KEY:prod").unwrap(), "prod_val"); // untouched
}
