//! Phase 3 exit criterion: the CoW B+tree behaves exactly like a `BTreeMap`
//! under randomized insert/delete/lookup/range, stays structurally valid after
//! every step, and survives commit + reopen. Reproducible from a seed.
#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use btree::{BTree, Direction};
use common::{MemoryBackend, Rng, SeededRng};
use pager::{PageId, Pager};

/// A small key drawn from a tiny alphabet so collisions, overwrites, splits,
/// and merges all happen frequently.
fn gen_key(rng: &SeededRng) -> Vec<u8> {
    let r = rng.next_u64();
    let len = (r % 3) as usize + 1;
    let mut k = Vec::with_capacity(len);
    let mut x = r >> 2;
    for _ in 0..len {
        k.push(b'a' + (x % 7) as u8);
        x /= 7;
    }
    k
}

/// A value whose length varies widely, so leaves overflow at different fills and
/// the tree grows several levels deep.
fn gen_val(rng: &SeededRng) -> Vec<u8> {
    let r = rng.next_u64();
    let len = (r % 600) as usize;
    let byte = (r >> 16) as u8;
    vec![byte; len]
}

/// Free every superseded page (safe because this test pins no old snapshot).
fn reclaim(pager: &Pager<MemoryBackend>, freed: &[PageId]) {
    for &id in freed {
        pager.free(id).unwrap();
    }
}

fn full_scan(tree: &BTree<MemoryBackend>, root: PageId, dir: Direction) -> Vec<(Vec<u8>, Vec<u8>)> {
    tree.range_dir(root, dir, None, None)
        .unwrap()
        .collect_all()
        .unwrap()
}

fn run(seed: u64, ops: usize) {
    let mut pager = Pager::create(MemoryBackend::new()).unwrap();
    let mut root = BTree::new(&pager).create().unwrap();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let rng = SeededRng::new(seed);

    for step in 0..ops {
        let tree = BTree::new(&pager);
        match rng.next_u64() % 100 {
            // Insert / overwrite.
            r if r < 55 => {
                let k = gen_key(&rng);
                let v = gen_val(&rng);
                let edit = tree.insert(root, &k, &v).unwrap();
                reclaim(&pager, &edit.freed);
                root = edit.new_root;
                model.insert(k, v);
            }
            // Delete (often a present key, sometimes absent).
            r if r < 85 => {
                let k = if !model.is_empty() && r.is_multiple_of(2) {
                    let nth = (rng.next_u64() as usize) % model.len();
                    model.keys().nth(nth).unwrap().clone()
                } else {
                    gen_key(&rng)
                };
                let edit = tree.delete(root, &k).unwrap();
                reclaim(&pager, &edit.freed);
                root = edit.new_root;
                model.remove(&k);
            }
            // Point lookups must match the model exactly.
            _ => {
                let k = gen_key(&rng);
                assert_eq!(
                    tree.lookup(root, &k).unwrap().as_deref(),
                    model.get(&k).map(Vec::as_slice),
                    "seed {seed} step {step}: lookup mismatch for {k:?}"
                );
            }
        }

        // The tree is valid and counts agree after every mutation.
        let stats = tree.validate(root).unwrap();
        assert_eq!(
            stats.entries,
            model.len() as u64,
            "seed {seed} step {step}: entry count drift"
        );

        // Occasionally cross-check ordered iteration and a bounded range.
        if step % 50 == 0 {
            let forward = full_scan(&tree, root, Direction::Forward);
            let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            assert_eq!(forward, expected, "seed {seed} step {step}: forward scan");

            let backward = full_scan(&tree, root, Direction::Backward);
            let mut rev = expected.clone();
            rev.reverse();
            assert_eq!(backward, rev, "seed {seed} step {step}: backward scan");

            let mut lo = gen_key(&rng);
            let mut hi = gen_key(&rng);
            if lo > hi {
                std::mem::swap(&mut lo, &mut hi);
            }
            let got = tree
                .range(root, Some(&lo), Some(&hi))
                .unwrap()
                .collect_all()
                .unwrap();
            let want: Vec<_> = model
                .range(lo.clone()..hi.clone())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            assert_eq!(
                got, want,
                "seed {seed} step {step}: bounded range [{lo:?},{hi:?})"
            );
        }

        // Periodically make the tree durable and sometimes reopen the file.
        if step % 200 == 199 {
            pager.commit().unwrap();
            if rng.next_u64().is_multiple_of(2) {
                let backend = pager.into_backend();
                pager = Pager::open(backend).unwrap();
                // The committed root survives the round-trip unchanged.
                let reopened = BTree::new(&pager);
                let scan = full_scan(&reopened, root, Direction::Forward);
                let expected: Vec<_> = model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                assert_eq!(scan, expected, "seed {seed} step {step}: scan after reopen");
            }
        }
    }
}

#[test]
fn matches_btreemap_under_random_ops() {
    for seed in [1u64, 2, 7, 42, 1234, 99999] {
        run(seed, 1500);
    }
}
