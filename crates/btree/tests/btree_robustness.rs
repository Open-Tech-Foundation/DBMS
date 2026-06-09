//! Phase 3 exit criterion: the node decoder rejects hostile/damaged page bytes
//! with typed errors and never panics, and oversized entries are refused.
#![allow(clippy::unwrap_used)]

use btree::{BTree, BTreeError};
use common::{IoBackend, MemoryBackend, Rng, SeededRng};
use pager::{PageType, Pager, HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE};

#[test]
fn corrupt_node_bytes_decode_to_typed_errors() {
    for seed in [1u64, 9, 77, 5005, 808080] {
        // Build a small tree so there is at least one real node at page 2.
        let pager = Pager::create(MemoryBackend::new()).unwrap();
        let tree = BTree::new(&pager);
        let mut root = tree.create().unwrap();
        for i in 0u8..20 {
            let edit = tree.insert(root, &[i], b"v").unwrap();
            root = edit.new_root;
        }
        pager.commit().unwrap();
        let backend = pager.into_backend();

        // Scribble random bytes over the root page's payload (header/checksum
        // left intact would be caught by the pager; here we corrupt the whole
        // page so either the pager or the node decoder must reject it).
        let rng = SeededRng::new(seed);
        let mut junk = vec![0u8; PAGE_PAYLOAD_SIZE];
        for b in junk.iter_mut() {
            *b = rng.next_u64() as u8;
        }
        backend
            .write_at(root.get() * PAGE_SIZE as u64 + HEADER_SIZE as u64, &junk)
            .unwrap();

        let reopened = Pager::open(backend).unwrap();
        let tree = BTree::new(&reopened);
        // A read of the damaged root must fail cleanly, never panic.
        let result = tree.lookup(root, b"\x00");
        assert!(result.is_err(), "seed {seed}: corrupt node was accepted");
    }
}

#[test]
fn decoding_arbitrary_payloads_never_panics() {
    // Feed the pager-stored payload straight from random bytes and ensure every
    // tree read returns a typed error rather than crashing.
    for seed in 0u64..64 {
        let pager = Pager::create(MemoryBackend::new()).unwrap();
        let id = pager.alloc().unwrap();
        let rng = SeededRng::new(seed.wrapping_mul(2654435761));
        let len = (rng.next_u64() as usize) % PAGE_PAYLOAD_SIZE;
        let mut payload = vec![0u8; len];
        for b in payload.iter_mut() {
            *b = rng.next_u64() as u8;
        }
        pager.write_page(id, PageType::Data, &payload).unwrap();

        let tree = BTree::new(&pager);
        // Whatever the bytes are, lookup/validate either succeed or error — the
        // point is they return, not panic.
        let _ = tree.lookup(id, b"anything");
        let _ = tree.validate(id);
    }
}

#[test]
fn oversized_entry_is_rejected() {
    let pager = Pager::create(MemoryBackend::new()).unwrap();
    let tree = BTree::new(&pager);
    let root = tree.create().unwrap();

    let huge = vec![0u8; PAGE_PAYLOAD_SIZE]; // far larger than a node cell allows
    let err = tree.insert(root, b"k", &huge).unwrap_err();
    assert!(matches!(err, BTreeError::EntryTooLarge { .. }));
}
