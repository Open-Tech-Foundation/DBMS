//! Phase 2 exit criterion: a crash (injected I/O fault) anywhere around the
//! commit's meta swap must leave the database recoverable — reopening yields
//! exactly the last fully-committed state, never a torn one (PLAN §6, §3.6).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use common::{FaultInjectingBackend, FaultPoint, MemoryBackend};
use pager::{PageId, PageType, Pager, HEADER_SIZE};

type Shared = Arc<FaultInjectingBackend<MemoryBackend>>;

const V1: &[u8] = b"committed-v1";
const V2: &[u8] = b"uncommitted-v2";

/// Create a database, commit one data page (page 2 = `V1`) as txn 1, and return
/// the live pager. No fault is armed yet.
fn build_committed(backend: Shared) -> Pager<Shared> {
    let pager = Pager::create(backend).unwrap();
    let id = pager.alloc().unwrap();
    assert_eq!(id, PageId::new(2));
    pager.write_page(id, PageType::Data, V1).unwrap();
    pager.commit().unwrap();
    assert_eq!(pager.txn_id(), 1);
    pager
}

/// Stage a second transaction (append page 3 = `V2`), then attempt to commit it
/// with a fault armed at `point`/`occurrence`. The commit must fail; reopening
/// must then land on a consistent txn 1 or txn 2 with page 2 always intact.
fn crash_during_second_commit(point: FaultPoint, occurrence: u64) {
    let backend: Shared = Arc::new(FaultInjectingBackend::new(MemoryBackend::new()));
    let pager = build_committed(Arc::clone(&backend));

    let id = pager.alloc().unwrap();
    assert_eq!(id, PageId::new(3));
    pager.write_page(id, PageType::Data, V2).unwrap();

    // Count occurrences from here so the fault targets the commit precisely.
    backend.reset_counters();
    backend.arm(point, occurrence);
    let result = pager.commit();
    assert!(
        result.is_err(),
        "{point:?}#{occurrence}: commit should have tripped the fault"
    );
    backend.disarm();
    drop(pager);

    // Reopen from the same bytes and prove the state is whole.
    let reopened = Pager::open(Arc::clone(&backend)).unwrap();
    let stats = reopened.validate().unwrap();
    let txn = reopened.txn_id();
    assert!(
        txn == 1 || txn == 2,
        "{point:?}#{occurrence}: reopened at unexpected txn {txn}"
    );

    // Page 2 survives every crash, in every recovered state.
    let p2 = reopened.read_page(PageId::new(2)).unwrap();
    assert_eq!(
        &p2[HEADER_SIZE..HEADER_SIZE + V1.len()],
        V1,
        "{point:?}#{occurrence}: page 2 content lost"
    );

    if txn == 1 {
        // The second transaction is absent: page 3 is not part of the file.
        assert_eq!(stats.page_count, 3, "{point:?}#{occurrence}");
        assert!(
            reopened.read_page(PageId::new(3)).is_err(),
            "{point:?}#{occurrence}: page 3 visible at txn 1"
        );
    } else {
        // The second transaction is fully present.
        assert_eq!(stats.page_count, 4, "{point:?}#{occurrence}");
        let p3 = reopened.read_page(PageId::new(3)).unwrap();
        assert_eq!(
            &p3[HEADER_SIZE..HEADER_SIZE + V2.len()],
            V2,
            "{point:?}#{occurrence}: page 3 content torn"
        );
    }
}

#[test]
fn crash_on_data_sync_recovers_previous_commit() {
    // The data-page fsync fails before the meta is ever touched.
    crash_during_second_commit(FaultPoint::Sync, 1);
}

#[test]
fn crash_on_meta_write_recovers_previous_commit() {
    // Data is durable, but the new meta page write fails: must stay at txn 1.
    crash_during_second_commit(FaultPoint::Write, 2);
}

#[test]
fn crash_on_meta_sync_recovers_consistently() {
    // The meta page reached the inactive slot but its fsync failed: reopening
    // may adopt txn 2 (slot valid) or txn 1, but never a half-written meta.
    crash_during_second_commit(FaultPoint::Sync, 2);
}
