//! Durability: a crash at any fsync boundary of a commit recovers cleanly —
//! every previously committed transaction survives, and the interrupted one is
//! either fully present or fully absent, never torn. An acknowledged commit is
//! always durable; an unacknowledged one may land either way (fsync ambiguity)
//! but never partially.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use common::{FaultInjectingBackend, FaultPoint, MemoryBackend};
use txn::Db;

type Shared = Arc<FaultInjectingBackend<MemoryBackend>>;

/// The committed baseline every trial starts from.
fn baseline() -> Vec<(Vec<u8>, Vec<u8>)> {
    (0u8..20).map(|i| (vec![b'k', i], vec![i; 32])).collect()
}

/// Build a database with the baseline committed, on a fresh fault backend.
fn with_baseline() -> Shared {
    let backend: Shared = Arc::new(FaultInjectingBackend::new(MemoryBackend::new()));
    let db = Db::create(Arc::clone(&backend)).unwrap();
    for (k, v) in baseline() {
        db.put(k, v).unwrap();
    }
    drop(db);
    backend
}

/// Reopen and read the whole database back.
fn reopened_scan(backend: Shared) -> Vec<(Vec<u8>, Vec<u8>)> {
    let db = Db::open(backend).unwrap();
    db.snapshot().scan().unwrap()
}

#[test]
fn crash_at_any_commit_fsync_boundary_recovers_consistently() {
    let base = baseline();
    let extra = (b"zzz".to_vec(), b"new-value".to_vec());

    // A fresh-page commit syncs twice (data, then meta) and writes the meta
    // once; trip each occurrence in turn. Some occurrences fall past the
    // commit's I/O, in which case the write simply succeeds.
    for point in [FaultPoint::Sync, FaultPoint::Write] {
        for occurrence in 1u64..=6 {
            let backend = with_baseline();
            backend.reset_counters();
            backend.arm(point, occurrence);

            let db = Db::open(Arc::clone(&backend)).unwrap();
            let result = db.put(extra.0.clone(), extra.1.clone());
            drop(db);
            backend.disarm();

            let scan = reopened_scan(Arc::clone(&backend));

            // The committed baseline is always fully intact.
            for (k, v) in &base {
                let found = scan.iter().find(|(sk, _)| sk == k).map(|(_, sv)| sv);
                assert_eq!(found, Some(v), "{point:?}#{occurrence}: baseline key lost");
            }
            // A commit that reported success must be durable. A commit that
            // reported failure must be atomic — fully present or fully absent
            // — but may legitimately be present: if the crash hit the final
            // meta fsync, the meta bytes may already have reached the disk
            // (the classic fsync ambiguity), and recovery rightly honors them.
            let has_extra = scan.iter().any(|(k, _)| k == &extra.0);
            if result.is_ok() {
                assert!(
                    has_extra,
                    "{point:?}#{occurrence}: acknowledged commit lost on recovery"
                );
            }
            if has_extra {
                let found = scan
                    .iter()
                    .find(|(k, _)| k == &extra.0)
                    .map(|(_, v)| v.clone());
                assert_eq!(
                    found.as_deref(),
                    Some(&extra.1[..]),
                    "{point:?}#{occurrence}: extra key present but value torn"
                );
            }
            // No phantom rows.
            let expected_len = base.len() + usize::from(has_extra);
            assert_eq!(
                scan.len(),
                expected_len,
                "{point:?}#{occurrence}: row count drift"
            );
        }
    }
}

#[test]
fn committed_transactions_survive_a_clean_reopen() {
    let backend = with_baseline();
    let scan = reopened_scan(Arc::clone(&backend));
    assert_eq!(scan, baseline());
}
