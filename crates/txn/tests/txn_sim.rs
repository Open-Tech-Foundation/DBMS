//! Deterministic simulation: seeded random interleavings of write
//! transactions and snapshot open/close/read, checked step by step against an
//! in-memory model. Every run is exactly reproducible from its seed.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use common::{MemoryBackend, Rng, SeededRng};
use txn::{Db, Op, Snapshot};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;
type Backend = Arc<MemoryBackend>;

const SEEDS: [u64; 4] = [1, 2, 3, 4];
const STEPS: u32 = 1200;
const KEY_SPACE: u64 = 40;
const MAX_OPEN_SNAPSHOTS: usize = 8;

fn rand_key(rng: &SeededRng) -> Vec<u8> {
    let i = rng.next_u64() % KEY_SPACE;
    format!("key-{i:02}").into_bytes()
}

fn rand_value(rng: &SeededRng, step: u32) -> Vec<u8> {
    let len = 1 + (rng.next_u64() % 200) as usize;
    let tag = (rng.next_u64() % 251) as u8;
    let mut v = vec![tag; len];
    v[0] = (step % 251) as u8;
    v
}

fn rand_op(rng: &SeededRng, step: u32) -> Op {
    if rng.next_u64().is_multiple_of(4) {
        Op::Delete(rand_key(rng))
    } else {
        Op::Put(rand_key(rng), rand_value(rng, step))
    }
}

/// Apply a transaction's ops to the model, in order.
fn apply_to_model(model: &mut Model, ops: &[Op]) {
    for op in ops {
        match op {
            Op::Put(k, v) => {
                model.insert(k.clone(), v.clone());
            }
            Op::Delete(k) => {
                model.remove(k);
            }
        }
    }
}

fn model_range(model: &Model, lo: Option<&[u8]>, hi: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    model
        .iter()
        .filter(|(k, _)| lo.is_none_or(|lo| k.as_slice() >= lo))
        .filter(|(k, _)| hi.is_none_or(|hi| k.as_slice() < hi))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Check a snapshot against the model state it was pinned at: a point lookup,
/// a random range, and (occasionally) the full scan.
fn check_view(rng: &SeededRng, snap: &Snapshot<Backend>, frozen: &Model, full: bool) {
    let probe = rand_key(rng);
    assert_eq!(snap.get(&probe).unwrap(), frozen.get(&probe).cloned());

    let mut lo = rand_key(rng);
    let mut hi = rand_key(rng);
    if lo > hi {
        std::mem::swap(&mut lo, &mut hi);
    }
    assert_eq!(
        snap.range(Some(&lo), Some(&hi)).unwrap(),
        model_range(frozen, Some(&lo), Some(&hi))
    );

    if full {
        let expected: Vec<_> = frozen.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(snap.scan().unwrap(), expected);
    }
}

fn run(seed: u64) {
    let rng = SeededRng::new(seed);
    let backend: Backend = Arc::new(MemoryBackend::new());
    let db = Db::create(Arc::clone(&backend)).unwrap();

    let mut model = Model::new();
    let mut snaps: Vec<(Snapshot<Backend>, Model)> = Vec::new();
    let mut last_txn = db.txn_id();

    for step in 0..STEPS {
        match rng.next_u64() % 10 {
            // Write transactions (single- and multi-op).
            0..=4 => {
                let count = 1 + (rng.next_u64() % 5) as usize;
                let ops: Vec<Op> = (0..count).map(|_| rand_op(&rng, step)).collect();
                let txn = db.write(ops.clone()).unwrap();
                assert!(txn >= last_txn, "seed {seed} step {step}: txn id went back");
                last_txn = txn;
                apply_to_model(&mut model, &ops);
            }
            // A transaction with one invalid op: rejected whole, model untouched.
            5 => {
                let ops = vec![
                    rand_op(&rng, step),
                    Op::Put(rand_key(&rng), vec![0u8; 8192]),
                ];
                assert!(
                    db.write(ops).is_err(),
                    "seed {seed} step {step}: oversized op was accepted"
                );
            }
            // Open a snapshot, pinning the current model state with it.
            6 => {
                if snaps.len() < MAX_OPEN_SNAPSHOTS {
                    snaps.push((db.snapshot(), model.clone()));
                }
            }
            // Close a random open snapshot.
            7 => {
                if !snaps.is_empty() {
                    let i = (rng.next_u64() as usize) % snaps.len();
                    snaps.swap_remove(i);
                }
            }
            // Check a random open snapshot against its frozen model.
            8 => {
                if !snaps.is_empty() {
                    let i = (rng.next_u64() as usize) % snaps.len();
                    let (snap, frozen) = &snaps[i];
                    check_view(&rng, snap, frozen, step.is_multiple_of(7));
                }
            }
            // Check the latest state through a fresh snapshot.
            _ => {
                let fresh = db.snapshot();
                assert!(fresh.txn_id() >= last_txn);
                check_view(&rng, &fresh, &model, step.is_multiple_of(7));
            }
        }
    }

    // Final sweep: every still-open snapshot holds its exact frozen view.
    for (snap, frozen) in &snaps {
        check_view(&rng, snap, frozen, true);
    }
    let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(db.snapshot().scan().unwrap(), expected);
    db.validate().unwrap();

    // Crash-free reopen recovers the final committed state exactly.
    snaps.clear();
    drop(db);
    let db = Db::open(backend).unwrap();
    assert_eq!(
        db.snapshot().scan().unwrap(),
        expected,
        "seed {seed}: reopen lost state"
    );
}

#[test]
fn seeded_interleavings_match_the_model() {
    for seed in SEEDS {
        run(seed);
    }
}
