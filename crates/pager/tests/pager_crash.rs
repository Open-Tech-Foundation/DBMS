//! Phase 2 exit criterion: a crash (injected I/O fault) anywhere around the
//! commit's meta swap must leave the database recoverable — reopening yields
//! exactly the last fully-committed state, never a torn one (PLAN §6, §3.6).
#![allow(clippy::unwrap_used)]

use std::collections::HashSet;
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

// --- crash injection over the free-list *rebuild* path -----------------------
//
// The commits above carry an empty free-list. This section crashes a commit
// that actually rebuilds a **multi-trunk** free-list. Every commit serializes
// the free set into fresh trunk pages, drawing the container pages from the free
// pool and leaving the previously committed trunks untouched on disk — so a
// crash before the meta swap must recover the *prior* commit's free-list whole.
//
// Contract note: the pager reuses free pages (as containers or via `alloc`) by
// overwriting them, which is crash-safe only for pages the last committed meta
// already treats as free. The real driver (`txn::writer`) upholds this: pages a
// CoW commit supersedes are parked in its `pending` list and returned to
// `pager.free()` only after that commit is durable (reclaimed past the snapshot
// watermark) — never freed-and-reused inside the same interruptible commit. So
// this test drives the rebuild the same way: the crashing commit only allocates
// from the *already-committed* free pool; it never frees a page that is still
// live in the prior meta.

/// Tag a page's payload with its own id, so any content drift is detectable.
fn tag_page(pager: &Pager<Shared>, id: PageId) {
    pager
        .write_page(id, PageType::Data, &id.get().to_le_bytes())
        .unwrap();
}

fn assert_self_tagged(pager: &Pager<Shared>, id: u64, ctx: &str) {
    let frame = pager.read_page(PageId::new(id)).unwrap();
    assert_eq!(
        &frame[HEADER_SIZE..HEADER_SIZE + 8],
        &id.to_le_bytes(),
        "{ctx}: live page {id} content torn"
    );
}

/// Build a committed state whose free-list spans **≥2 trunks**, then crash a
/// later commit that rebuilds it. Reopening must land on a whole state — the
/// prior commit (free-list intact) or the new one (fully applied) — with
/// `validate()` clean either way, the committed-live pages never torn, and the
/// recovered free-list never handing back a still-live page.
fn crash_during_freelist_rebuild(point: FaultPoint, occurrence: u64) {
    let backend: Shared = Arc::new(FaultInjectingBackend::new(MemoryBackend::new()));
    let pager = Pager::create(Arc::clone(&backend)).unwrap();

    // Commit A: 700 live pages, then durably free the first 600 (committed), so
    // the last committed free-list needs >1 trunk (508-id capacity). The 100
    // survivors (pages 602..=701) are the only committed-live pages.
    let ids: Vec<PageId> = (0..700)
        .map(|_| {
            let id = pager.alloc().unwrap();
            tag_page(&pager, id);
            id
        })
        .collect();
    pager.commit().unwrap();
    for &id in &ids[..600] {
        pager.free(id).unwrap();
    }
    pager.commit().unwrap();
    let stats_a = pager.validate().unwrap();
    assert!(
        stats_a.trunk_count >= 2,
        "{point:?}#{occurrence}: need a multi-trunk free-list, got {stats_a:?}"
    );
    let txn_a = pager.txn_id();
    let live_a: HashSet<u64> = ids[600..].iter().map(|id| id.get()).collect();

    // Stage commit B: allocate 40 pages from the *already-committed* free pool
    // (drawn from the 600 durably-free pages) and write them. This shrinks the
    // pool and forces `rebuild_free_list` to write a fresh trunk chain from
    // durably-free container pages — never touching a page live in meta A.
    let b_new: Vec<PageId> = (0..40)
        .map(|_| {
            let id = pager.alloc().unwrap();
            tag_page(&pager, id);
            id
        })
        .collect();

    // Arm the fault and attempt the rebuild commit.
    backend.reset_counters();
    backend.arm(point, occurrence);
    let crashed = pager.commit().is_err();
    backend.disarm();
    drop(pager);

    // Reopen from the on-disk bytes: the free-list must be structurally whole.
    let reopened = Pager::open(Arc::clone(&backend)).unwrap();
    reopened.validate().unwrap();
    let txn = reopened.txn_id();
    let ctx = format!("{point:?}#{occurrence}");
    assert!(
        txn == txn_a || txn == txn_a + 1,
        "{ctx}: reopened at unexpected txn {txn}"
    );

    // The committed-live survivors are preserved under every outcome — the
    // rebuild's container reuse only ever touched durably-free pages.
    for &id in &live_a {
        assert_self_tagged(&reopened, id, &ctx);
    }

    let recovered_live: HashSet<u64> = if txn == txn_a {
        // Rolled back: a non-crashing commit would have advanced the txn. The
        // newly allocated pages are free again; only the survivors are live.
        assert!(crashed, "{ctx}: rolled back to A but commit returned Ok");
        live_a
    } else {
        // Advanced to B: the newly written pages are durable and read back.
        for &id in &b_new {
            assert_self_tagged(&reopened, id.get(), &ctx);
        }
        live_a
            .iter()
            .copied()
            .chain(b_new.iter().map(|id| id.get()))
            .collect()
    };

    // Whichever state recovered, the free-list is usable and never aliases a
    // live page: allocate a batch and confirm none collides with the live set.
    for _ in 0..40 {
        let id = reopened.alloc().unwrap();
        assert!(
            !recovered_live.contains(&id.get()),
            "{ctx}: alloc handed out still-live page {id:?}"
        );
        tag_page(&reopened, id);
    }
    reopened.commit().unwrap();
    reopened.validate().unwrap();
}

#[test]
fn crash_across_the_freelist_rebuild_writes_recovers_whole() {
    // Sweep a fault across every page write of the rebuild commit — the trunk
    // pages, the data pages, and the meta write. Occurrences past the last
    // write simply don't trip (commit succeeds); the helper handles both.
    for occurrence in 1..=48 {
        crash_during_freelist_rebuild(FaultPoint::Write, occurrence);
    }
}

#[test]
fn crash_at_each_sync_of_the_freelist_rebuild_recovers_whole() {
    // Sync #1 is the data/trunk durability point (recovery stays at A); sync #2
    // is the meta durability point (recovery may adopt A or B).
    for occurrence in 1..=2 {
        crash_during_freelist_rebuild(FaultPoint::Sync, occurrence);
    }
}
