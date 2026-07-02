//! Phase 2 exit criterion: randomized alloc/free/read/write keeps the
//! free-list, page contents, and checksums consistent — across commits and
//! reopens. Reproducible from a seed (PLAN §3.6).
#![allow(clippy::unwrap_used)]

use std::collections::{HashMap, HashSet};

use common::{MemoryBackend, Rng, SeededRng};
use pager::{PageId, PageType, Pager, HEADER_SIZE};

/// Encode a content tag into a page payload.
fn payload_for(tag: u64) -> [u8; 8] {
    tag.to_le_bytes()
}

/// Read the content tag back from a page frame.
fn tag_of(frame: &[u8]) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&frame[HEADER_SIZE..HEADER_SIZE + 8]);
    u64::from_le_bytes(b)
}

fn pick(rng: &SeededRng, ids: &[u64]) -> u64 {
    ids[(rng.next_u64() % ids.len() as u64) as usize]
}

fn verify_live(pager: &Pager<MemoryBackend>, live: &HashMap<u64, u64>, seed: u64) {
    for (&id, &tag) in live {
        let frame = pager.read_page(PageId::new(id)).unwrap();
        assert_eq!(
            tag_of(&frame[..]),
            tag,
            "seed {seed}: page {id} content drift"
        );
    }
    pager.validate().unwrap();
}

fn run(seed: u64, ops: usize) {
    let mut pager = Pager::create(MemoryBackend::new()).unwrap();
    let rng = SeededRng::new(seed);
    let mut live: HashMap<u64, u64> = HashMap::new();
    let mut next_tag = 1u64;

    for _ in 0..ops {
        match rng.next_u64() % 100 {
            // Allocate and initialize a fresh page.
            r if r < 55 || live.is_empty() => {
                let id = pager.alloc().unwrap();
                let tag = next_tag;
                next_tag += 1;
                pager
                    .write_page(id, PageType::Data, &payload_for(tag))
                    .unwrap();
                live.insert(id.get(), tag);
            }
            // Free a random live page.
            r if r < 78 => {
                let ids: Vec<u64> = live.keys().copied().collect();
                let victim = pick(&rng, &ids);
                pager.free(PageId::new(victim)).unwrap();
                live.remove(&victim);
            }
            // Overwrite a random live page.
            r if r < 90 => {
                let ids: Vec<u64> = live.keys().copied().collect();
                let target = pick(&rng, &ids);
                let tag = next_tag;
                next_tag += 1;
                pager
                    .write_page(PageId::new(target), PageType::Data, &payload_for(tag))
                    .unwrap();
                live.insert(target, tag);
            }
            // Commit, verify, and sometimes reopen.
            _ => {
                pager.commit().unwrap();
                verify_live(&pager, &live, seed);
                if rng.next_u64().is_multiple_of(2) {
                    let backend = pager.into_backend();
                    pager = Pager::open(backend).unwrap();
                    verify_live(&pager, &live, seed);
                }
            }
        }
    }

    pager.commit().unwrap();
    verify_live(&pager, &live, seed);
}

#[test]
fn randomized_alloc_free_read_write_stays_consistent() {
    for seed in [1u64, 7, 42, 1234, 99999] {
        run(seed, 3000);
    }
}

#[test]
fn freelist_reuses_pages_across_multiple_trunks() {
    let pager = Pager::create(MemoryBackend::new()).unwrap();

    // Allocate a thousand pages (well past one trunk's 508-id capacity).
    let mut ids = Vec::new();
    for _ in 0..1000 {
        let id = pager.alloc().unwrap();
        pager
            .write_page(id, PageType::Data, &payload_for(1))
            .unwrap();
        ids.push(id);
    }
    pager.commit().unwrap();
    let page_count_before = pager.validate().unwrap().page_count;

    // Free them all; the free list must span more than one trunk.
    for &id in &ids {
        pager.free(id).unwrap();
    }
    pager.commit().unwrap();
    let stats = pager.validate().unwrap();
    assert!(
        stats.trunk_count >= 2,
        "expected multiple trunks, got {stats:?}"
    );

    // Reallocating reuses the freed pages. A few of them are spent as trunk
    // *containers* (structural, drawn from the freed pool and recycled one
    // commit later), so reuse is complete up to that bounded trunk overhead —
    // never an unbounded growth of the file.
    let trunk_count = stats.trunk_count;
    let original: HashSet<u64> = ids.iter().map(|i| i.get()).collect();
    let mut fresh = 0u64;
    for _ in 0..1000 {
        let id = pager.alloc().unwrap();
        if !original.contains(&id.get()) {
            fresh += 1;
        }
    }
    pager.commit().unwrap();
    assert!(
        fresh <= trunk_count,
        "reuse should cover all but the {trunk_count} trunk pages, but {fresh} were fresh"
    );
    assert!(
        pager.validate().unwrap().page_count <= page_count_before + trunk_count,
        "the file must not grow beyond the trunk overhead"
    );

    // Steady state: after a warm-up (the high-water settles a couple of pages
    // above peak demand to cover the trunk overhead), repeated free/alloc
    // cycles hold the file size flat — the parked trunk pages cycle back through
    // the free set each commit, so there is no drift or leak.
    let cycle = |round: u64| {
        let live: Vec<PageId> = (0..1000).map(|_| pager.alloc().unwrap()).collect();
        for &id in &live {
            pager
                .write_page(id, PageType::Data, &payload_for(round))
                .unwrap();
        }
        pager.commit().unwrap();
        for &id in &live {
            pager.free(id).unwrap();
        }
        pager.commit().unwrap();
    };
    for round in 0..3 {
        cycle(round);
    }
    let steady = pager.validate().unwrap().page_count;
    for round in 3..8 {
        cycle(round);
    }
    assert_eq!(
        pager.validate().unwrap().page_count,
        steady,
        "repeated free/alloc cycles must not grow the file past steady state"
    );
}
