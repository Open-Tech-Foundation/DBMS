//! Functional coverage for the transaction layer: writes, snapshot reads,
//! snapshot isolation, atomic multi-op transactions, and recovery on reopen.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use common::MemoryBackend;
use txn::{Db, Op};

#[test]
fn writes_are_readable_and_survive_reopen() {
    let backend = Arc::new(MemoryBackend::new());
    let db = Db::create(Arc::clone(&backend)).unwrap();

    db.put(b"a".to_vec(), b"1".to_vec()).unwrap();
    db.put(b"b".to_vec(), b"2".to_vec()).unwrap();
    db.put(b"c".to_vec(), b"3".to_vec()).unwrap();
    db.delete(b"b".to_vec()).unwrap();

    let snap = db.snapshot();
    assert_eq!(snap.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(snap.get(b"b").unwrap(), None);
    assert_eq!(
        snap.scan().unwrap(),
        vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ]
    );
    drop(snap);
    drop(db);

    // Reopen the same bytes: the last committed state is recovered exactly.
    let db = Db::open(Arc::clone(&backend)).unwrap();
    let snap = db.snapshot();
    assert_eq!(snap.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
    assert_eq!(snap.get(b"b").unwrap(), None);
    assert_eq!(snap.get(b"c").unwrap().as_deref(), Some(&b"3"[..]));
}

#[test]
fn a_snapshot_does_not_see_later_writes() {
    let db = Db::create(MemoryBackend::new()).unwrap();
    db.put(b"k".to_vec(), b"v0".to_vec()).unwrap();

    let before = db.snapshot();
    let v0_txn = before.txn_id();

    db.put(b"k".to_vec(), b"v1".to_vec()).unwrap();
    db.put(b"new".to_vec(), b"x".to_vec()).unwrap();

    // The old snapshot is frozen at its version.
    assert_eq!(before.get(b"k").unwrap().as_deref(), Some(&b"v0"[..]));
    assert_eq!(before.get(b"new").unwrap(), None);
    assert_eq!(before.txn_id(), v0_txn);

    // A fresh snapshot sees the new state.
    let after = db.snapshot();
    assert_eq!(after.get(b"k").unwrap().as_deref(), Some(&b"v1"[..]));
    assert_eq!(after.get(b"new").unwrap().as_deref(), Some(&b"x"[..]));
    assert!(after.txn_id() > v0_txn);
}

#[test]
fn a_transaction_is_atomic_all_or_nothing() {
    let db = Db::create(MemoryBackend::new()).unwrap();
    db.put(b"keep".to_vec(), b"yes".to_vec()).unwrap();

    // One op is invalid (value far larger than a node can hold): the whole
    // transaction must be rejected, leaving nothing behind.
    let huge = vec![0u8; 8192];
    let result = db.write(vec![
        Op::Put(b"x".to_vec(), b"1".to_vec()),
        Op::Put(b"y".to_vec(), huge),
        Op::Put(b"z".to_vec(), b"3".to_vec()),
    ]);
    assert!(result.is_err());

    let snap = db.snapshot();
    assert_eq!(snap.get(b"x").unwrap(), None, "partial transaction leaked");
    assert_eq!(snap.get(b"z").unwrap(), None, "partial transaction leaked");
    assert_eq!(snap.get(b"keep").unwrap().as_deref(), Some(&b"yes"[..]));
}

#[test]
fn multi_op_transaction_commits_together() {
    let db = Db::create(MemoryBackend::new()).unwrap();
    let txn = db
        .write(vec![
            Op::Put(b"a".to_vec(), b"1".to_vec()),
            Op::Put(b"b".to_vec(), b"2".to_vec()),
            Op::Delete(b"a".to_vec()),
            Op::Put(b"c".to_vec(), b"3".to_vec()),
        ])
        .unwrap();

    let snap = db.snapshot();
    assert_eq!(snap.txn_id(), txn);
    assert_eq!(
        snap.scan().unwrap(),
        vec![
            (b"b".to_vec(), b"2".to_vec()),
            (b"c".to_vec(), b"3".to_vec()),
        ]
    );
}
