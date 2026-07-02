//! Functional coverage for the transaction layer: writes, snapshot reads,
//! snapshot isolation, atomic multi-op transactions, and recovery on reopen.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use common::{CategorizedError, ErrorCategory, IoBackend, MemoryBackend};
use txn::{Db, JobDb, Op, TxnError, WriteCtx, WriteJob};

/// A non-fatal rejection error for the mutate-then-fail test job.
#[derive(Debug)]
struct Boom;

impl std::fmt::Display for Boom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("boom")
    }
}

impl std::error::Error for Boom {}

impl CategorizedError for Boom {
    fn category(&self) -> ErrorCategory {
        ErrorCategory::Constraint
    }
}

/// A write job that inserts a key (allocating fresh pages) and then, if
/// `reject` is set, rejects — violating validate-then-apply on purpose. A
/// rejected instance is the worst case for page reclamation: the pages it
/// allocated are unpublished and must be returned to the allocator.
struct Mutate {
    key: Vec<u8>,
    reject: bool,
}

impl<B: IoBackend> WriteJob<B> for Mutate {
    type Out = ();

    fn apply(self, ctx: &mut WriteCtx<'_, B>) -> txn::Result<()> {
        let root = ctx.insert(ctx.root(), &self.key, &[0u8; 64])?;
        ctx.set_root(root);
        if self.reject {
            Err(TxnError::Rejected(Box::new(Boom)))
        } else {
            Ok(())
        }
    }
}

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
fn a_rejected_transaction_does_not_desync_reclamation() {
    // Regression: the writer used to reclaim parked pages at the start of
    // every batch — including one whose only transaction is rejected. Such a
    // batch never commits, leaving the in-memory freelist ahead of the disk
    // (caught by `validate()` as freelist corruption).
    let db = Db::create(MemoryBackend::new()).unwrap();
    db.put(b"k".to_vec(), vec![1u8; 64]).unwrap();
    // Supersede pages so the next committing batch has work to park/reclaim.
    db.put(b"k".to_vec(), vec![2u8; 64]).unwrap();
    db.put(b"k".to_vec(), vec![3u8; 64]).unwrap();

    // A rejected transaction in its own batch (oversized value).
    assert!(db
        .write(vec![Op::Put(b"x".to_vec(), vec![0u8; 8192])])
        .is_err());
    db.validate().expect("pager state must match the disk");

    // And the database keeps working normally afterwards.
    db.put(b"k2".to_vec(), b"v".to_vec()).unwrap();
    db.validate().unwrap();
    assert_eq!(
        db.snapshot().get(b"k").unwrap().as_deref(),
        Some(&[3u8; 64][..])
    );
}

#[test]
fn a_rejected_mutating_transaction_reclaims_its_pages() {
    // A transaction that mutates and *then* fails leaves the CoW pages it
    // allocated unpublished. The writer must return them to the allocator (at
    // the next commit) so a stream of failed transactions cannot grow the file
    // without bound.
    let db: JobDb<_, Mutate> = JobDb::create(MemoryBackend::new()).unwrap();
    let commit = |k: &[u8]| {
        db.submit(Mutate {
            key: k.to_vec(),
            reject: false,
        })
        .unwrap();
    };
    let fail = || {
        assert!(db
            .submit(Mutate {
                key: b"x".to_vec(),
                reject: true,
            })
            .is_err());
    };

    // A streak of failed transactions followed by a commit that reclaims their
    // orphaned pages. Run it once to reach the allocator's steady state.
    for _ in 0..100 {
        fail();
    }
    commit(b"a");
    commit(b"a"); // settle reclamation of the prior commit's superseded pages
    db.validate().unwrap();
    let steady = db.validate().unwrap().page_count;

    // A second, identical streak reuses the pages freed at the first commit
    // rather than extending the file. Were the orphans leaked, another 100
    // failures would add ~100 pages; the only permitted growth is a page or
    // two of reclamation lag.
    for _ in 0..100 {
        fail();
    }
    commit(b"b");
    commit(b"b");
    let after = db.validate().unwrap().page_count;
    assert!(
        after <= steady + 3,
        "rejected transactions leaked pages: grew from {steady} to {after} over 100 failures"
    );

    // And committed data is intact.
    let snap = db.snapshot();
    assert_eq!(snap.get(b"a").unwrap().as_deref(), Some(&[0u8; 64][..]));
    assert_eq!(snap.get(b"b").unwrap().as_deref(), Some(&[0u8; 64][..]));
    assert_eq!(snap.get(b"x").unwrap(), None);
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
