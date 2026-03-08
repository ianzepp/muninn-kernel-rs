use tokio::sync::mpsc;

use crate::sigcall::SigcallRegistry;

fn registry_and_tx() -> (SigcallRegistry, mpsc::Sender<crate::frame::Frame>) {
    let reg = SigcallRegistry::new();
    let (tx, _rx) = mpsc::channel(16);
    (reg, tx)
}

#[test]
fn register_and_lookup() {
    let (reg, tx) = registry_and_tx();
    reg.register("window:create", "displayd", tx).unwrap();

    assert!(reg.lookup("window:create").is_some());
    assert!(reg.lookup("window:delete").is_none());
}

#[test]
fn register_same_owner_updates() {
    let (reg, tx1) = registry_and_tx();
    let (tx2, _rx2) = mpsc::channel(16);

    reg.register("ai:task", "agent-1", tx1).unwrap();
    reg.register("ai:task", "agent-1", tx2).unwrap();

    assert_eq!(reg.len(), 1);
}

#[test]
fn register_different_owner_fails() {
    let (reg, tx1) = registry_and_tx();
    let (tx2, _rx2) = mpsc::channel(16);

    reg.register("ai:task", "agent-1", tx1).unwrap();
    let err = reg.register("ai:task", "agent-2", tx2).unwrap_err();

    assert!(matches!(
        err,
        crate::error::SigcallError::AlreadyRegistered { .. }
    ));
}

#[test]
fn register_reserved_prefix_fails() {
    let (reg, tx) = registry_and_tx();
    let err = reg.register("sigcall:register", "sneaky", tx).unwrap_err();
    assert!(matches!(err, crate::error::SigcallError::Reserved { .. }));
}

#[test]
fn unregister_by_owner() {
    let (reg, tx) = registry_and_tx();
    reg.register("window:create", "displayd", tx).unwrap();
    reg.unregister("window:create", "displayd").unwrap();

    assert!(reg.lookup("window:create").is_none());
    assert!(reg.is_empty());
}

#[test]
fn unregister_wrong_owner_fails() {
    let (reg, tx) = registry_and_tx();
    reg.register("window:create", "displayd", tx).unwrap();

    let err = reg.unregister("window:create", "other").unwrap_err();
    assert!(matches!(err, crate::error::SigcallError::NotOwner { .. }));
}

#[test]
fn unregister_nonexistent_fails() {
    let reg = SigcallRegistry::new();
    let err = reg.unregister("window:create", "displayd").unwrap_err();
    assert!(matches!(
        err,
        crate::error::SigcallError::NotRegistered { .. }
    ));
}

#[test]
fn unregister_all_removes_only_owner() {
    let (reg, tx1) = registry_and_tx();
    let (tx2, _rx2) = mpsc::channel(16);

    reg.register("ai:task", "agent-1", tx1).unwrap();
    reg.register("window:create", "displayd", tx2).unwrap();

    reg.unregister_all("agent-1");

    assert!(reg.lookup("ai:task").is_none());
    assert!(reg.lookup("window:create").is_some());
    assert_eq!(reg.len(), 1);
}

#[test]
fn unregister_all_on_empty_is_noop() {
    let reg = SigcallRegistry::new();
    reg.unregister_all("nobody");
    assert!(reg.is_empty());
}

#[test]
fn list_returns_all_entries() {
    let (reg, tx1) = registry_and_tx();
    let (tx2, _rx2) = mpsc::channel(16);

    reg.register("ai:task", "agent-1", tx1).unwrap();
    reg.register("window:create", "displayd", tx2).unwrap();

    let mut entries = reg.list();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0], ("ai:task".into(), "agent-1".into()));
    assert_eq!(entries[1], ("window:create".into(), "displayd".into()));
}

#[test]
fn is_empty_and_len() {
    let (reg, tx) = registry_and_tx();
    assert!(reg.is_empty());
    assert_eq!(reg.len(), 0);

    reg.register("test:op", "owner", tx).unwrap();
    assert!(!reg.is_empty());
    assert_eq!(reg.len(), 1);
}

#[test]
fn clone_shares_state() {
    let (reg, tx) = registry_and_tx();
    let reg2 = reg.clone();

    reg.register("test:op", "owner", tx).unwrap();
    assert!(reg2.lookup("test:op").is_some());
}
