//! Phase 2 exit criterion: the page decoder and meta-recovery path reject
//! hostile or damaged bytes with typed errors and never panic (PLAN §6, §3.6).
#![allow(clippy::unwrap_used)]

use common::{IoBackend, MemoryBackend, Rng, SeededRng};
use pager::{PageType, Pager, HEADER_SIZE, PAGE_SIZE};

/// A page-sized buffer of pseudo-random bytes derived from `seed`.
fn junk_page(seed: u64) -> Vec<u8> {
    let rng = SeededRng::new(seed);
    let mut bytes = vec![0u8; PAGE_SIZE];
    for chunk in bytes.chunks_mut(8) {
        let n = chunk.len();
        chunk.copy_from_slice(&rng.next_u64().to_le_bytes()[..n]);
    }
    bytes
}

#[test]
fn corrupted_data_page_is_rejected_not_panicked() {
    for seed in [1u64, 2, 3, 1000, 424242] {
        let pager = Pager::create(MemoryBackend::new()).unwrap();
        let id = pager.alloc().unwrap();
        pager.write_page(id, PageType::Data, b"intact").unwrap();
        pager.commit().unwrap();
        let backend = pager.into_backend();

        // Overwrite page 2 with random bytes.
        backend
            .write_at(id.get() * PAGE_SIZE as u64, &junk_page(seed))
            .unwrap();

        let reopened = Pager::open(backend).unwrap();
        assert!(
            reopened.read_page(id).is_err(),
            "seed {seed}: corrupt page read did not error"
        );
    }
}

#[test]
fn both_meta_slots_corrupt_fails_to_open() {
    let pager = Pager::create(MemoryBackend::new()).unwrap();
    let id = pager.alloc().unwrap();
    pager.write_page(id, PageType::Data, b"intact").unwrap();
    pager.commit().unwrap();
    let backend = pager.into_backend();

    backend.write_at(0, &junk_page(11)).unwrap();
    backend.write_at(PAGE_SIZE as u64, &junk_page(22)).unwrap();

    assert!(
        Pager::open(backend).is_err(),
        "open should fail when no valid meta survives"
    );
}

#[test]
fn corrupt_active_meta_recovers_from_the_other_slot() {
    let pager = Pager::create(MemoryBackend::new()).unwrap();

    // txn 1 writes page 2 into the file and lands in meta slot 1.
    let p2 = pager.alloc().unwrap();
    pager.write_page(p2, PageType::Data, b"alpha").unwrap();
    pager.commit().unwrap();

    // txn 2 appends page 3 and lands in meta slot 0 (the active slot).
    let p3 = pager.alloc().unwrap();
    pager.write_page(p3, PageType::Data, b"beta").unwrap();
    pager.commit().unwrap();
    assert_eq!(pager.txn_id(), 2);

    let backend = pager.into_backend();

    // Destroy the active slot (slot 0). Recovery must fall back to slot 1.
    backend.write_at(0, &junk_page(7)).unwrap();

    let reopened = Pager::open(backend).unwrap();
    let stats = reopened.validate().unwrap();
    assert_eq!(
        reopened.txn_id(),
        1,
        "should recover the older valid commit"
    );
    assert_eq!(stats.active_slot, 1);
    assert_eq!(stats.page_count, 3, "txn 2's page 3 is excluded");

    // Page 2's committed content is still readable under the recovered meta.
    let frame = reopened.read_page(p2).unwrap();
    assert_eq!(&frame[HEADER_SIZE..HEADER_SIZE + 5], b"alpha");
    // Page 3 belongs only to the lost txn 2 and is out of range now.
    assert!(reopened.read_page(p3).is_err());
}
